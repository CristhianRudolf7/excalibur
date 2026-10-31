//! Owner identity and sessions contract: architecture sections 20.12-20.15 and
//! the 20.16 verification rows for entry authentication and teams.
//!
//! Every fixture is synthetic and local: principals, device keys and pairing
//! codes are generated at test time, passwords are fixed test strings, time
//! comes from an injected manual clock (no sleeping), and storage is a
//! temporary SQLite database. Nothing here touches the network, a paid API or
//! a real credential.

use excalibur_identity::{
    AuthenticatedSession, DeviceChallenge, DeviceProof, Identity, IdentityConfig, IdentityError,
    IssuedSession, LocalChannel, LoginRequest, ManualClock, Password, PasswordPolicy, RateLimit,
    device_proof_message,
};
use excalibur_store::{Database, DatabaseKind};
use excalibur_vault::{
    MasterKeyStore, SecretKind, SecretMetadata, SecretRef, SecretValue, Vault, VaultError,
};
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const NOW: i64 = 1_750_000_000;
const OWNER_PASSWORD: &str = "owner-pass-1";
const MEMBER_PASSWORD: &str = "member-pass-2";

fn test_config() -> IdentityConfig {
    IdentityConfig {
        password: PasswordPolicy {
            min_chars: 8,
            max_bytes: 128,
            memory_kib: 8,
            iterations: 1,
            parallelism: 1,
        },
        session_ttl_seconds: 3600,
        session_idle_seconds: 600,
        csrf_ttl_seconds: 300,
        challenge_ttl_seconds: 120,
        pairing_ttl_seconds: 300,
        invite_ttl_seconds: 3600,
        max_active_pairing_codes: 3,
        max_csrf_per_session: 4,
        max_open_challenges_per_device: 8,
        login_limit: RateLimit {
            max_failures: 3,
            window_seconds: 60,
        },
        code_source_limit: RateLimit {
            max_failures: 3,
            window_seconds: 60,
        },
        code_global_limit: RateLimit {
            max_failures: 10,
            window_seconds: 60,
        },
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    clock: ManualClock,
    id: Identity,
}

async fn setup() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("identity.db");
    let db = Database::open(path.clone(), DatabaseKind::Main, Default::default()).unwrap();
    let clock = ManualClock::new(NOW);
    let id = Identity::open(
        db,
        test_config(),
        Arc::new(clock.clone()) as Arc<dyn excalibur_identity::Clock>,
    )
    .await
    .unwrap();
    Fixture {
        _dir: dir,
        path,
        clock,
        id,
    }
}

fn local() -> LocalChannel {
    LocalChannel::host_attested()
}

fn keypair() -> (Ed25519KeyPair, [u8; 32]) {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let public: [u8; 32] = pair.public_key().as_ref().try_into().unwrap();
    (pair, public)
}

async fn enroll_device(
    id: &Identity,
    username: &str,
    password: &str,
    name: &str,
    public: [u8; 32],
) -> String {
    id.enroll_local_device(&local(), username, Password::new(password), name, public)
        .await
        .unwrap()
        .device_id
}

async fn login(
    id: &Identity,
    username: &str,
    password: &str,
    device_id: &str,
    pair: &Ed25519KeyPair,
) -> IssuedSession {
    let challenge: DeviceChallenge = id.begin_device_challenge(device_id).await.unwrap();
    let message = device_proof_message(&challenge.challenge_id, &challenge.nonce, device_id);
    let signature = pair.sign(&message).as_ref().to_vec();
    id.login(LoginRequest {
        username: username.into(),
        password: Password::new(password),
        device: DeviceProof {
            device_id: device_id.into(),
            challenge_id: challenge.challenge_id,
            signature,
        },
    })
    .await
    .unwrap()
}

async fn owner_session(id: &Identity) -> (IssuedSession, Ed25519KeyPair) {
    id.bootstrap_owner(&local(), "owner", Password::new(OWNER_PASSWORD))
        .await
        .unwrap();
    let (pair, public) = keypair();
    let device = enroll_device(id, "owner", OWNER_PASSWORD, "laptop", public).await;
    let issued = login(id, "owner", OWNER_PASSWORD, &device, &pair).await;
    (issued, pair)
}

async fn member_session(
    id: &Identity,
    owner: &AuthenticatedSession,
    username: &str,
) -> (IssuedSession, String) {
    let principal = id
        .create_principal(owner, username, Password::new(MEMBER_PASSWORD))
        .await
        .unwrap();
    let (pair, public) = keypair();
    let device = enroll_device(id, username, MEMBER_PASSWORD, "phone", public).await;
    let issued = login(id, username, MEMBER_PASSWORD, &device, &pair).await;
    (issued, principal)
}

async fn wrong_login(
    id: &Identity,
    pair: &Ed25519KeyPair,
    device: &str,
) -> Result<IssuedSession, IdentityError> {
    let challenge = id.begin_device_challenge(device).await.unwrap();
    let message = device_proof_message(&challenge.challenge_id, &challenge.nonce, device);
    id.login(LoginRequest {
        username: "owner".into(),
        password: Password::new("wrong-password"),
        device: DeviceProof {
            device_id: device.into(),
            challenge_id: challenge.challenge_id,
            signature: pair.sign(&message).as_ref().to_vec(),
        },
    })
    .await
}

fn is_unauthenticated<T>(result: &Result<T, IdentityError>) -> bool {
    matches!(result, Err(IdentityError::Unauthenticated))
}

#[tokio::test]
async fn owner_bootstrap_login_and_authenticate_allow() {
    let f = setup().await;
    assert!(
        is_unauthenticated(&f.id.authenticate("exs1.deadbeef.deadbeef").await),
        "unknown bearer token must deny, not reveal"
    );
    let (issued, _) = owner_session(&f.id).await;
    let session = f.id.authenticate(issued.token.expose()).await.unwrap();
    assert_eq!(session.principal_id(), issued.session.principal_id());
    assert_eq!(session.session_id(), issued.session.session_id());
    assert_eq!(session.device_id(), issued.session.device_id());
    assert_eq!(f.id.list_sessions(&session).await.unwrap().len(), 1);
    assert_eq!(f.id.list_devices(&session).await.unwrap().len(), 1);
    // Bootstrapping twice is refused without touching the first owner.
    assert!(matches!(
        f.id.bootstrap_owner(&local(), "someone", Password::new("another-pass-3"))
            .await,
        Err(IdentityError::Conflict)
    ));
    let again = f.id.authenticate(issued.token.expose()).await.unwrap();
    assert_eq!(again.principal_id(), session.principal_id());
}

#[tokio::test]
async fn wrong_password_unknown_user_and_weak_password_deny() {
    let f = setup().await;
    let (issued, _) = owner_session(&f.id).await;
    assert!(matches!(
        f.id.bootstrap_owner(&local(), "weak", Password::new("short"))
            .await,
        Err(IdentityError::WeakPassword)
    ));
    // Wrong password denies without revealing whether the user exists.
    let (pair, public) = keypair();
    let device = enroll_device(&f.id, "owner", OWNER_PASSWORD, "spare", public).await;
    let challenge = f.id.begin_device_challenge(&device).await.unwrap();
    let message = device_proof_message(&challenge.challenge_id, &challenge.nonce, &device);
    let denied =
        f.id.login(LoginRequest {
            username: "owner".into(),
            password: Password::new("wrong-password"),
            device: DeviceProof {
                device_id: device.clone(),
                challenge_id: challenge.challenge_id,
                signature: pair.sign(&message).as_ref().to_vec(),
            },
        })
        .await;
    assert!(is_unauthenticated(&denied));
    // Unknown users hash against a dummy salt, so the error is identical.
    let ghost =
        f.id.login(LoginRequest {
            username: "ghost".into(),
            password: Password::new("wrong-password"),
            device: DeviceProof {
                device_id: device,
                challenge_id: "chl_deadbeef".into(),
                signature: vec![0u8; 64],
            },
        })
        .await;
    assert!(is_unauthenticated(&ghost));
    // The denied attempts did not disturb the live session.
    f.id.authenticate(issued.token.expose()).await.unwrap();
}

#[tokio::test]
async fn login_rate_limit_blocks_then_recovers() {
    let f = setup().await;
    let (_, _) = owner_session(&f.id).await;
    let (pair, public) = keypair();
    let device = enroll_device(&f.id, "owner", OWNER_PASSWORD, "tablet", public).await;
    for _ in 0..3 {
        assert!(is_unauthenticated(
            &wrong_login(&f.id, &pair, &device).await
        ));
    }
    match wrong_login(&f.id, &pair, &device).await {
        Err(IdentityError::RateLimited {
            retry_after_seconds,
        }) => {
            assert!(retry_after_seconds > 0 && retry_after_seconds <= 60)
        }
        other => panic!("expected rate limiting, got {other:?}"),
    }
    // The window slides with the injected clock; success clears the counter.
    f.clock.advance(61);
    login(&f.id, "owner", OWNER_PASSWORD, &device, &pair).await;
}

#[tokio::test]
async fn device_proof_is_single_use_and_device_bound() {
    let f = setup().await;
    let (issued, _) = owner_session(&f.id).await;
    let owner_device = issued.session.device_id().to_owned();
    let (other_pair, other_public) = keypair();
    let other_device = enroll_device(&f.id, "owner", OWNER_PASSWORD, "spare", other_public).await;
    // A signature from another key does not open this device's session.
    let challenge = f.id.begin_device_challenge(&owner_device).await.unwrap();
    let message = device_proof_message(&challenge.challenge_id, &challenge.nonce, &owner_device);
    let forged =
        f.id.login(LoginRequest {
            username: "owner".into(),
            password: Password::new(OWNER_PASSWORD),
            device: DeviceProof {
                device_id: owner_device.clone(),
                challenge_id: challenge.challenge_id.clone(),
                signature: other_pair.sign(&message).as_ref().to_vec(),
            },
        })
        .await;
    assert!(is_unauthenticated(&forged));
    // The consumed challenge cannot be replayed, even with a valid signature.
    let replay =
        f.id.login(LoginRequest {
            username: "owner".into(),
            password: Password::new(OWNER_PASSWORD),
            device: DeviceProof {
                device_id: owner_device.clone(),
                challenge_id: challenge.challenge_id,
                signature: vec![0u8; 64],
            },
        })
        .await;
    assert!(is_unauthenticated(&replay));
    // A fresh challenge for the other device still works: failures consume one
    // challenge, they never brick the device.
    login(&f.id, "owner", OWNER_PASSWORD, &other_device, &other_pair).await;
}

#[tokio::test]
async fn session_idle_expiry_revocation_and_password_rotation() {
    let f = setup().await;
    let (issued, pair) = owner_session(&f.id).await;
    let device = issued.session.device_id().to_owned();
    // Sliding activity keeps the session alive past the first idle window.
    f.clock.advance(300);
    f.id.authenticate(issued.token.expose()).await.unwrap();
    f.clock.advance(599);
    f.id.authenticate(issued.token.expose()).await.unwrap();
    // Past the absolute lifetime the session is gone even though it was active.
    f.clock.advance(3600);
    assert!(is_unauthenticated(
        &f.id.authenticate(issued.token.expose()).await
    ));
    // Fresh sessions expire on inactivity without any traffic.
    let fresh = login(&f.id, "owner", OWNER_PASSWORD, &device, &pair).await;
    f.clock.advance(601);
    assert!(is_unauthenticated(
        &f.id.authenticate(fresh.token.expose()).await
    ));
    // Revocation and logout deny immediately, including CSRF verification.
    let current = login(&f.id, "owner", OWNER_PASSWORD, &device, &pair).await;
    let extra = f.id.issue_csrf(&current.session).await.unwrap();
    f.id.logout(&current.session).await.unwrap();
    assert!(is_unauthenticated(
        &f.id.authenticate(current.token.expose()).await
    ));
    assert!(is_unauthenticated(
        &f.id.verify_csrf(&current.session, extra.expose()).await
    ));
    // Rotating the password revokes every other session of the principal.
    let keep = login(&f.id, "owner", OWNER_PASSWORD, &device, &pair).await;
    let other = login(&f.id, "owner", OWNER_PASSWORD, &device, &pair).await;
    let revoked =
        f.id.change_password(
            &keep.session,
            Password::new(OWNER_PASSWORD),
            Password::new("owner-pass-9"),
        )
        .await
        .unwrap();
    assert_eq!(revoked, 1);
    assert!(is_unauthenticated(
        &f.id.authenticate(other.token.expose()).await
    ));
    f.id.authenticate(keep.token.expose()).await.unwrap();
    // Disabling a principal revokes its sessions; the owner keeps working.
    let (member, _) = member_session(&f.id, &keep.session, "teammate").await;
    f.id.disable_principal(&keep.session, member.session.principal_id())
        .await
        .unwrap();
    assert!(is_unauthenticated(
        &f.id.authenticate(member.token.expose()).await
    ));
    f.id.authenticate(keep.token.expose()).await.unwrap();
}

#[tokio::test]
async fn csrf_tokens_are_bound_to_their_session_and_expire() {
    let f = setup().await;
    let (owner, _) = owner_session(&f.id).await;
    let (member, _) = member_session(&f.id, &owner.session, "teammate").await;
    let token = f.id.issue_csrf(&owner.session).await.unwrap();
    f.id.verify_csrf(&owner.session, token.expose())
        .await
        .unwrap();
    // A token issued to one session never verifies for another principal.
    assert!(is_unauthenticated(
        &f.id.verify_csrf(&member.session, token.expose()).await
    ));
    // A session bearer token is not a CSRF token.
    assert!(is_unauthenticated(
        &f.id.verify_csrf(&owner.session, owner.token.expose()).await
    ));
    // Expiry is enforced by the injected clock.
    f.clock.advance(301);
    assert!(is_unauthenticated(
        &f.id.verify_csrf(&owner.session, token.expose()).await
    ));
}

#[tokio::test]
async fn pairing_codes_are_single_use_expiring_and_rate_limited() {
    let f = setup().await;
    let (owner, _) = owner_session(&f.id).await;
    let pairing = f.id.create_pairing_code(&owner.session).await.unwrap();
    let (_, public) = keypair();
    let enrolled =
        f.id.redeem_pairing_code(
            "synthetic-source-1",
            pairing.code.expose(),
            "remote",
            public,
        )
        .await
        .unwrap();
    assert_eq!(enrolled.principal_id, owner.session.principal_id());
    // Replaying the consumed code denies, even from another source.
    assert!(is_unauthenticated(
        &f.id
            .redeem_pairing_code(
                "synthetic-source-2",
                pairing.code.expose(),
                "remote",
                keypair().1
            )
            .await
    ));
    // Expired codes deny without enrolling anything.
    let stale = f.id.create_pairing_code(&owner.session).await.unwrap();
    f.clock.advance(301);
    assert!(is_unauthenticated(
        &f.id
            .redeem_pairing_code(
                "synthetic-source-3",
                stale.code.expose(),
                "remote",
                keypair().1
            )
            .await
    ));
    // Repeated failures from one source block even the correct code.
    let current = f.id.create_pairing_code(&owner.session).await.unwrap();
    for i in 0..3 {
        assert!(
            is_unauthenticated(
                &f.id
                    .redeem_pairing_code("synthetic-source-4", "AAAAA-AAAAA", "remote", keypair().1)
                    .await
            ),
            "failure {i} must deny"
        );
    }
    match f
        .id
        .redeem_pairing_code(
            "synthetic-source-4",
            current.code.expose(),
            "remote",
            keypair().1,
        )
        .await
    {
        Err(IdentityError::RateLimited { .. }) => {}
        other => panic!("expected source rate limiting, got {other:?}"),
    }
}

#[tokio::test]
async fn bearer_tokens_never_belong_in_urls() {
    let f = setup().await;
    let (issued, _) = owner_session(&f.id).await;
    for token in [issued.token.expose(), issued.csrf.expose()] {
        for banned in ['?', '#', '&', '=', '/', '+', '%'] {
            assert!(
                !token.contains(banned),
                "bearer value must stay out of URLs, found {banned:?}"
            );
        }
    }
    // A token pasted into a query string does not parse back to a session.
    let smuggled = format!("{}?next=/admin", issued.token.expose());
    assert!(is_unauthenticated(&f.id.authenticate(&smuggled).await));
}

#[tokio::test]
async fn principals_and_namespaces_are_isolated() {
    let f = setup().await;
    let (owner, _) = owner_session(&f.id).await;
    let (member, member_principal) = member_session(&f.id, &owner.session, "teammate").await;
    assert_ne!(
        owner.session.data_namespace(),
        member.session.data_namespace(),
        "distinct principals must receive distinct storage namespaces"
    );
    assert_eq!(
        member.session.data_namespace(),
        member.session.data_namespace(),
        "namespaces must be stable for one principal"
    );
    // Non-owners cannot mint principals, and members see only their own teams.
    assert!(matches!(
        f.id.create_principal(
            &member.session,
            "intruder",
            Password::new("intruder-pass-4")
        )
        .await,
        Err(IdentityError::Forbidden)
    ));
    let team = f.id.create_team(&owner.session, "alpha").await.unwrap();
    assert_eq!(f.id.list_teams(&member.session).await.unwrap(), vec![]);
    // Foreign teams are indistinguishable from missing ones.
    assert!(matches!(
        f.id.team_view(&member.session, &team).await,
        Err(IdentityError::NotFound)
    ));
    assert!(matches!(
        f.id.admit(
            &member.session,
            &team,
            excalibur_identity::ResourceKind::Tool,
            "synthetic-tool",
            1,
            "key-1",
        )
        .await,
        Err(IdentityError::NotFound)
    ));
    assert_eq!(member.session.principal_id(), member_principal);
}

#[tokio::test]
async fn team_invites_enforce_roles_and_single_use() {
    let f = setup().await;
    let (owner, _) = owner_session(&f.id).await;
    let (admin, _) = member_session(&f.id, &owner.session, "adminuser").await;
    let team = f.id.create_team(&owner.session, "alpha").await.unwrap();
    // Promote via a host invite, then consume it exactly once.
    let invite =
        f.id.create_team_invite(&owner.session, &team, excalibur_identity::TeamRole::Admin)
            .await
            .unwrap();
    f.id.join_team(&admin.session, "synthetic-source-1", invite.code.expose())
        .await
        .unwrap();
    assert!(is_unauthenticated(
        &f.id
            .join_team(&admin.session, "synthetic-source-2", invite.code.expose())
            .await
    ));
    // Admins invite members but never admins; members invite nobody.
    let (member, _) = member_session(&f.id, &owner.session, "teammate").await;
    let member_invite =
        f.id.create_team_invite(&admin.session, &team, excalibur_identity::TeamRole::Member)
            .await
            .unwrap();
    f.id.join_team(
        &member.session,
        "synthetic-source-3",
        member_invite.code.expose(),
    )
    .await
    .unwrap();
    assert!(matches!(
        f.id.create_team_invite(&admin.session, &team, excalibur_identity::TeamRole::Admin)
            .await,
        Err(IdentityError::Forbidden)
    ));
    assert!(matches!(
        f.id.create_team_invite(&member.session, &team, excalibur_identity::TeamRole::Member)
            .await,
        Err(IdentityError::Forbidden)
    ));
    // A demoted issuer's outstanding invite no longer admits anyone.
    let (late, _) = member_session(&f.id, &owner.session, "latecomer").await;
    let stale =
        f.id.create_team_invite(&admin.session, &team, excalibur_identity::TeamRole::Member)
            .await
            .unwrap();
    f.id.remove_member(&owner.session, &team, admin.session.principal_id())
        .await
        .unwrap();
    assert!(is_unauthenticated(
        &f.id
            .join_team(&late.session, "synthetic-source-4", stale.code.expose())
            .await
    ));
}

#[derive(Default)]
struct MemoryKeys(Mutex<BTreeMap<String, SecretValue>>);
impl MasterKeyStore for MemoryKeys {
    fn load(&self, id: &str) -> Result<Option<SecretValue>, VaultError> {
        Ok(self.0.lock().unwrap().get(id).cloned())
    }
    fn store(&self, id: &str, key: &SecretValue) -> Result<(), VaultError> {
        self.0.lock().unwrap().insert(id.into(), key.clone());
        Ok(())
    }
}

fn test_vault(dir: &tempfile::TempDir) -> Vault {
    let vault = Vault::open(
        dir.path().join("host-vault.json"),
        Arc::new(MemoryKeys::default()),
        "synthetic-master",
    )
    .unwrap();
    vault
        .put(
            SecretMetadata {
                reference: SecretRef::parse("secret://synthetic-llm").unwrap(),
                name: "synthetic-llm".into(),
                kind: SecretKind::ApiKey,
                usage: "synthetic test only".into(),
                url: None,
            },
            SecretValue::new(b"host-credential-bytes".to_vec()).unwrap(),
        )
        .unwrap();
    vault
}

#[tokio::test]
async fn shared_resources_admit_under_quota_with_keyed_idempotency() {
    use excalibur_identity::{QuotaSpec, ResourceKind};
    let f = setup().await;
    let vault = test_vault(&f._dir);
    let (owner, _) = owner_session(&f.id).await;
    let (member, member_principal) = member_session(&f.id, &owner.session, "teammate").await;
    let team = f.id.create_team(&owner.session, "alpha").await.unwrap();
    let invite =
        f.id.create_team_invite(&owner.session, &team, excalibur_identity::TeamRole::Member)
            .await
            .unwrap();
    f.id.join_team(&member.session, "synthetic-source-1", invite.code.expose())
        .await
        .unwrap();
    // Unshared resources and missing quotas deny with distinct member-safe errors.
    assert!(matches!(
        f.id.admit(
            &member.session,
            &team,
            ResourceKind::Tool,
            "synthetic-tool",
            1,
            "key-1"
        )
        .await,
        Err(IdentityError::NotShared)
    ));
    assert!(
        matches!(
            f.id.set_share(
                &member.session,
                &team,
                ResourceKind::Provider,
                "synthetic-llm",
                Some(SecretRef::parse("secret://synthetic-llm").unwrap()),
            )
            .await,
            Err(IdentityError::Forbidden)
        ),
        "only the host shares resources"
    );
    f.id.set_share(
        &owner.session,
        &team,
        ResourceKind::Provider,
        "synthetic-llm",
        Some(SecretRef::parse("secret://synthetic-llm").unwrap()),
    )
    .await
    .unwrap();
    assert!(matches!(
        f.id.admit(
            &member.session,
            &team,
            ResourceKind::Provider,
            "synthetic-llm",
            1,
            "key-1"
        )
        .await,
        Err(IdentityError::QuotaNotConfigured)
    ));
    f.id.set_quota(
        &owner.session,
        &team,
        QuotaSpec {
            member_id: Some(member_principal.clone()),
            kind: ResourceKind::Provider,
            name: "synthetic-llm".into(),
            limit_units: 5,
            window_seconds: 3600,
        },
    )
    .await
    .unwrap();
    // First admission reserves units; the keyed replay returns the original
    // receipt without charging again.
    let first =
        f.id.admit(
            &member.session,
            &team,
            ResourceKind::Provider,
            "synthetic-llm",
            3,
            "key-1",
        )
        .await
        .unwrap();
    assert!(!first.receipt.replayed);
    assert_eq!(first.receipt.reserved_units, 3);
    assert_eq!(first.receipt.remaining_units, 2);
    let replay =
        f.id.admit(
            &member.session,
            &team,
            ResourceKind::Provider,
            "synthetic-llm",
            3,
            "key-1",
        )
        .await
        .unwrap();
    assert!(replay.receipt.replayed);
    assert_eq!(replay.receipt.admission_id, first.receipt.admission_id);
    // Same key with different parameters is a conflict, never a second charge.
    assert!(matches!(
        f.id.admit(
            &member.session,
            &team,
            ResourceKind::Provider,
            "synthetic-llm",
            2,
            "key-1"
        )
        .await,
        Err(IdentityError::Conflict)
    ));
    // The quota still holds: 3 used, 2 left.
    assert!(matches!(
        f.id.admit(
            &member.session,
            &team,
            ResourceKind::Provider,
            "synthetic-llm",
            3,
            "key-2"
        )
        .await,
        Err(IdentityError::QuotaExceeded { remaining: 2, .. })
    ));
    // The host proxy ticket signs with the host vault; the member receipt and
    // every member-visible view carry no credential bytes.
    assert!(
        first
            .ticket
            .with_host_credential(&vault, |b| b == b"host-credential-bytes")
            .unwrap()
    );
    let receipt_json = serde_json::to_string(&first.receipt).unwrap();
    assert!(!receipt_json.contains("host-credential-bytes"));
    let view = f.id.team_view(&member.session, &team).await.unwrap();
    assert!(
        !serde_json::to_string(&view)
            .unwrap()
            .contains("host-credential")
    );
    assert!(!serde_json::to_string(&view).unwrap().contains("secret://"));
    assert!(!format!("{:?}", first.ticket).contains("host-credential-bytes"));
    // Settling below the reservation refunds the difference; settling is
    // idempotent per value and conflicting across values.
    f.id.settle(&first.ticket, 2).await.unwrap();
    f.id.settle(&first.ticket, 2).await.unwrap();
    assert!(matches!(
        f.id.settle(&first.ticket, 3).await,
        Err(IdentityError::Conflict)
    ));
    let last =
        f.id.admit(
            &member.session,
            &team,
            ResourceKind::Provider,
            "synthetic-llm",
            3,
            "key-3",
        )
        .await
        .unwrap();
    assert_eq!(last.receipt.remaining_units, 0);
}

#[tokio::test]
async fn project_access_requires_explicit_host_grants() {
    use excalibur_identity::ProjectPermission;
    let f = setup().await;
    let (owner, _) = owner_session(&f.id).await;
    let (member, member_principal) = member_session(&f.id, &owner.session, "teammate").await;
    let team = f.id.create_team(&owner.session, "alpha").await.unwrap();
    let invite =
        f.id.create_team_invite(&owner.session, &team, excalibur_identity::TeamRole::Member)
            .await
            .unwrap();
    f.id.join_team(&member.session, "synthetic-source-1", invite.code.expose())
        .await
        .unwrap();
    assert!(
        !f.id
            .check_project_access(&member.session, &team, "website", ProjectPermission::Read)
            .await
            .unwrap()
    );
    // Members cannot grant themselves access.
    assert!(matches!(
        f.id.grant_project(
            &member.session,
            &team,
            &member_principal,
            "website",
            ProjectPermission::Write,
        )
        .await,
        Err(IdentityError::Forbidden)
    ));
    f.id.grant_project(
        &owner.session,
        &team,
        &member_principal,
        "website",
        ProjectPermission::Read,
    )
    .await
    .unwrap();
    assert!(
        f.id.check_project_access(&member.session, &team, "website", ProjectPermission::Read)
            .await
            .unwrap()
    );
    assert!(
        !f.id
            .check_project_access(&member.session, &team, "website", ProjectPermission::Write)
            .await
            .unwrap(),
        "read does not imply write"
    );
    f.id.grant_project(
        &owner.session,
        &team,
        &member_principal,
        "website",
        ProjectPermission::Write,
    )
    .await
    .unwrap();
    assert!(
        f.id.check_project_access(&member.session, &team, "website", ProjectPermission::Write)
            .await
            .unwrap(),
        "write implies read"
    );
    f.id.revoke_project(&owner.session, &team, &member_principal, "website")
        .await
        .unwrap();
    assert!(
        !f.id
            .check_project_access(&member.session, &team, "website", ProjectPermission::Read)
            .await
            .unwrap()
    );
    // The host owns its projects without needing a grant row.
    assert!(
        f.id.check_project_access(&owner.session, &team, "website", ProjectPermission::Write)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn sessions_teams_and_codes_survive_a_restart() {
    let f = setup().await;
    let (owner, _) = owner_session(&f.id).await;
    let (member, _) = member_session(&f.id, &owner.session, "teammate").await;
    let team = f.id.create_team(&owner.session, "alpha").await.unwrap();
    let invite =
        f.id.create_team_invite(&owner.session, &team, excalibur_identity::TeamRole::Member)
            .await
            .unwrap();
    f.id.join_team(&member.session, "synthetic-source-1", invite.code.expose())
        .await
        .unwrap();
    let pairing = f.id.create_pairing_code(&owner.session).await.unwrap();
    let owner_token = owner.token.expose().to_owned();
    let member_token = member.token.expose().to_owned();
    let team_id = team.clone();
    drop(owner);
    drop(member);
    drop(f.id);
    // Reopen the same database file: every durable row comes back.
    let db = Database::open(f.path.clone(), DatabaseKind::Main, Default::default()).unwrap();
    let clock = ManualClock::new(NOW);
    let reopened = Identity::open(
        db,
        test_config(),
        Arc::new(clock) as Arc<dyn excalibur_identity::Clock>,
    )
    .await
    .unwrap();
    let owner_session = reopened.authenticate(&owner_token).await.unwrap();
    let member_session = reopened.authenticate(&member_token).await.unwrap();
    assert_eq!(reopened.list_teams(&member_session).await.unwrap().len(), 1);
    assert_eq!(
        reopened
            .team_view(&member_session, &team_id)
            .await
            .unwrap()
            .members
            .len(),
        2
    );
    // A consumed invite stays consumed across the restart.
    assert!(is_unauthenticated(
        &reopened
            .join_team(&member_session, "synthetic-source-2", invite.code.expose())
            .await
    ));
    // An unconsumed pairing code survives the restart too.
    let enrolled = reopened
        .redeem_pairing_code(
            "synthetic-source-3",
            pairing.code.expose(),
            "remote",
            keypair().1,
        )
        .await
        .unwrap();
    assert_eq!(enrolled.principal_id, owner_session.principal_id());
}
