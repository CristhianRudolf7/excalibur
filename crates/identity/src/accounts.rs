//! Principals, passwords, devices, sessions, CSRF tokens and pairing codes.
use crate::crypto::{self, Argon2Cost, HASH_LEN, SALT_LEN};
use crate::{
    AuthenticatedSession, CsrfToken, Fail, Identity, IdentityError, LocalChannel,
    PairingCodeSecret, Password, PrincipalRole, Result, SessionToken, Tx, rate_blocked, rate_clear,
    rate_fail,
};
use excalibur_store::rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use zeroize::Zeroizing;

pub(crate) const SESSION_KIND: &str = "exs";
pub(crate) const CSRF_KIND: &str = "exc";
const LOGIN_SCOPE: &str = "login";
pub(crate) const CODE_SOURCE_SCOPE: &str = "code_source";
pub(crate) const CODE_GLOBAL_SCOPE: &str = "code_global";

/// Proof that the caller holds the private key of an enrolled device: an
/// Ed25519 signature over [`crate::device_proof_message`] for a fresh challenge.
#[derive(Clone, Debug)]
pub struct DeviceProof {
    pub device_id: String,
    pub challenge_id: String,
    pub signature: Vec<u8>,
}

#[derive(Debug)]
pub struct LoginRequest {
    pub username: String,
    pub password: Password,
    pub device: DeviceProof,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceChallenge {
    pub challenge_id: String,
    pub nonce: [u8; 32],
    pub expires_at: i64,
}

#[derive(Debug)]
pub struct IssuedSession {
    pub session: AuthenticatedSession,
    pub token: SessionToken,
    pub csrf: CsrfToken,
    pub idle_expires_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EnrolledDevice {
    pub device_id: String,
    pub principal_id: String,
}

#[derive(Debug)]
pub struct PairingCode {
    pub code: PairingCodeSecret,
    pub expires_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub device_id: String,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
    pub current: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeviceInfo {
    pub device_id: String,
    pub name: String,
    pub enrolled_via: String,
    pub created_at: i64,
    pub revoked: bool,
}

pub(crate) fn normalize_username(input: &str) -> Option<String> {
    let name = input.trim().to_ascii_lowercase();
    let bytes = name.as_bytes();
    let valid = (3..=64).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    valid.then_some(name)
}

pub(crate) fn valid_label(label: &str) -> bool {
    let chars = label.chars().count();
    (1..=128).contains(&chars) && !label.chars().any(char::is_control) && !label.trim().is_empty()
}

fn valid_public_key(key: &[u8; 32]) -> bool {
    key.iter().any(|b| *b != 0)
}

/// Stored Argon2id credential.
#[derive(Clone)]
struct Credential {
    principal_id: String,
    disabled: bool,
    cost: Argon2Cost,
    salt: Vec<u8>,
    hash: Zeroizing<Vec<u8>>,
    generation: i64,
}

struct NewHash {
    cost: Argon2Cost,
    salt: [u8; SALT_LEN],
    hash: Zeroizing<[u8; HASH_LEN]>,
}

/// Result of the pre-hash reservation step of a password attempt.
struct Reserved {
    credential: Option<Credential>,
    device_ok: bool,
}

struct Verified {
    principal_id: Option<String>,
    generation: i64,
    ok: bool,
    rehash: Option<NewHash>,
}

fn load_credential(c: &Connection, username: &str) -> Tx<Option<Credential>> {
    Ok(c.query_row(
        "SELECT p.id,p.disabled_at IS NOT NULL,w.m_cost,w.t_cost,w.p_cost,w.salt,w.hash,w.generation
         FROM identity_principals p JOIN identity_passwords w ON w.principal_id=p.id
         WHERE p.username=?1",
        [username],
        |r| {
            Ok(Credential {
                principal_id: r.get(0)?,
                disabled: r.get(1)?,
                cost: Argon2Cost {
                    m: r.get(2)?,
                    t: r.get(3)?,
                    p: r.get(4)?,
                },
                salt: r.get(5)?,
                hash: Zeroizing::new(r.get(6)?),
                generation: r.get(7)?,
            })
        },
    )
    .optional()?)
}

fn store_password(
    c: &Connection,
    principal_id: &str,
    new: &NewHash,
    generation: i64,
    now: i64,
) -> Tx<()> {
    c.execute(
        "INSERT INTO identity_passwords(principal_id,algorithm,version,m_cost,t_cost,p_cost,salt,hash,generation,updated_at)
         VALUES(?1,'argon2id',19,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(principal_id) DO UPDATE SET m_cost=excluded.m_cost,t_cost=excluded.t_cost,
           p_cost=excluded.p_cost,salt=excluded.salt,hash=excluded.hash,generation=excluded.generation,
           updated_at=excluded.updated_at",
        params![
            principal_id,
            new.cost.m,
            new.cost.t,
            new.cost.p,
            &new.salt[..],
            &new.hash[..],
            generation,
            now
        ],
    )?;
    Ok(())
}

/// Validates the session row, its device and its principal inside the caller's
/// transaction, so a revocation that commits first always wins.
pub(crate) fn live(c: &Connection, s: &AuthenticatedSession, now: i64) -> Tx<()> {
    let row: Option<(bool, i64, i64, bool, bool)> = c
        .query_row(
            "SELECT s.revoked_at IS NOT NULL,s.expires_at,s.idle_expires_at,
                    d.revoked_at IS NOT NULL,p.disabled_at IS NOT NULL
             FROM identity_sessions s
             JOIN identity_devices d ON d.id=s.device_id AND d.principal_id=s.principal_id
             JOIN identity_principals p ON p.id=s.principal_id
             WHERE s.id=?1 AND s.principal_id=?2 AND s.device_id=?3",
            params![s.session_id, s.principal_id, s.device_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    match row {
        Some((false, expires, idle, false, false)) if now < expires && now < idle => Ok(()),
        _ => Err(IdentityError::Unauthenticated.into()),
    }
}

#[derive(Clone, Copy)]
enum SessionsOf {
    Principal,
    Device,
}

pub(crate) fn revoke_sessions_of_principal(
    c: &Connection,
    id: &str,
    now: i64,
    reason: &str,
) -> Tx<usize> {
    revoke_sessions(c, SessionsOf::Principal, id, now, reason)
}

fn revoke_sessions(c: &Connection, of: SessionsOf, id: &str, now: i64, reason: &str) -> Tx<usize> {
    let column = match of {
        SessionsOf::Principal => "principal_id",
        SessionsOf::Device => "device_id",
    };
    let changed = c.execute(
        &format!(
            "UPDATE identity_sessions SET revoked_at=?1,revoke_reason=?2 WHERE revoked_at IS NULL AND {column}=?3"
        ),
        params![now, reason, id],
    )?;
    c.execute(
        &format!(
            "DELETE FROM identity_csrf WHERE session_id IN (SELECT id FROM identity_sessions WHERE {column}=?1)"
        ),
        [id],
    )?;
    Ok(changed)
}

impl Identity {
    fn check_policy(&self, password: &Password) -> Result<()> {
        let p = &self.config.password;
        let value = password.expose();
        if value.chars().count() < p.min_chars
            || value.len() > p.max_bytes
            || value.trim().is_empty()
        {
            return Err(IdentityError::WeakPassword);
        }
        Ok(())
    }

    async fn hash_new(&self, password: Password) -> Result<NewHash> {
        let cost = crypto::policy_cost(&self.config.password);
        tokio::task::spawn_blocking(move || {
            let salt = crypto::random::<SALT_LEN>()?;
            let hash = crypto::argon2_hash(password.expose().as_bytes(), &salt, cost)?;
            Ok(NewHash { cost, salt, hash })
        })
        .await
        .map_err(|_| IdentityError::Invalid("password hashing task failed"))?
    }

    /// Hashes off the async runtime. Unknown users hash against a dummy salt with
    /// the policy cost so timing does not reveal account existence.
    async fn verify_reserved(&self, reserved: &Reserved, password: Password) -> Result<Verified> {
        let policy = crypto::policy_cost(&self.config.password);
        let dummy_salt = self.dummy_salt;
        let credential = reserved.credential.clone();
        tokio::task::spawn_blocking(move || {
            let Some(cred) = credential else {
                let _ = crypto::argon2_hash(password.expose().as_bytes(), &dummy_salt, policy)?;
                return Ok(Verified {
                    principal_id: None,
                    generation: 0,
                    ok: false,
                    rehash: None,
                });
            };
            let computed =
                crypto::argon2_hash(password.expose().as_bytes(), &cred.salt, cred.cost)?;
            let ok = crypto::ct_eq(&computed[..], &cred.hash);
            let rehash = if ok && cred.cost != policy {
                let salt = crypto::random::<SALT_LEN>()?;
                let hash = crypto::argon2_hash(password.expose().as_bytes(), &salt, policy)?;
                Some(NewHash {
                    cost: policy,
                    salt,
                    hash,
                })
            } else {
                None
            };
            Ok(Verified {
                principal_id: Some(cred.principal_id),
                generation: cred.generation,
                ok: ok && !cred.disabled,
                rehash,
            })
        })
        .await
        .map_err(|_| IdentityError::Invalid("password hashing task failed"))?
    }

    /// Pessimistically counts the attempt as a failure before hashing, so
    /// concurrent guesses can never exceed the limit; success clears it later.
    /// A device proof's challenge is consumed here whatever the outcome.
    async fn reserve_attempt(
        &self,
        username: String,
        proof: Option<DeviceProof>,
    ) -> Result<Reserved> {
        let limit = self.config.login_limit;
        let now = self.now();
        self.tx(move |c| {
            if let Some(retry) = rate_blocked(c, LOGIN_SCOPE, &username, limit, now)? {
                return Ok(Err(IdentityError::RateLimited {
                    retry_after_seconds: retry,
                }));
            }
            rate_fail(c, LOGIN_SCOPE, &username, limit, now)?;
            let credential = load_credential(c, &username)?;
            let device_ok = match proof {
                None => true,
                Some(proof) => {
                    let consumed = c.execute(
                        "UPDATE identity_challenges SET consumed_at=?1
                         WHERE id=?2 AND device_id=?3 AND consumed_at IS NULL AND expires_at>?1",
                        params![now, proof.challenge_id, proof.device_id],
                    )? == 1;
                    let device: Option<(Vec<u8>, String, Vec<u8>)> = if consumed {
                        c.query_row(
                            "SELECT d.public_key,d.principal_id,ch.nonce FROM identity_devices d
                             JOIN identity_challenges ch ON ch.device_id=d.id
                             WHERE d.id=?1 AND ch.id=?2 AND d.revoked_at IS NULL",
                            params![proof.device_id, proof.challenge_id],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        )
                        .optional()?
                    } else {
                        None
                    };
                    device.is_some_and(|(key, owner, nonce)| {
                        credential
                            .as_ref()
                            .is_some_and(|cred| cred.principal_id == owner)
                            && crypto::verify_device_signature(
                                &key,
                                &crypto::device_proof_message(
                                    &proof.challenge_id,
                                    &nonce,
                                    &proof.device_id,
                                ),
                                &proof.signature,
                            )
                    })
                }
            };
            Ok(Ok(Reserved {
                credential,
                device_ok,
            }))
        })
        .await?
    }

    /// Completes a successful attempt in the caller's transaction: the stored
    /// credential must be unchanged, the principal enabled; clears the counter
    /// and applies a pending cost upgrade.
    fn accept_password(
        c: &Connection,
        username: &str,
        verified: &Verified,
        now: i64,
    ) -> Tx<String> {
        let Some(principal_id) = verified.principal_id.as_ref().filter(|_| verified.ok) else {
            return Err(IdentityError::Unauthenticated.into());
        };
        let current: Option<(i64, bool)> = c
            .query_row(
                "SELECT w.generation,p.disabled_at IS NOT NULL FROM identity_passwords w
                 JOIN identity_principals p ON p.id=w.principal_id WHERE w.principal_id=?1",
                [principal_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if current != Some((verified.generation, false)) {
            return Err(IdentityError::Unauthenticated.into());
        }
        rate_clear(c, LOGIN_SCOPE, username)?;
        if let Some(new) = &verified.rehash {
            store_password(c, principal_id, new, verified.generation + 1, now)?;
        }
        Ok(principal_id.clone())
    }

    /// Creates the first principal (the owner) over the host's local channel.
    /// Refused once any principal exists.
    pub async fn bootstrap_owner(
        &self,
        _local: &LocalChannel,
        username: &str,
        password: Password,
    ) -> Result<String> {
        let username = normalize_username(username).ok_or(IdentityError::Invalid("username"))?;
        self.check_policy(&password)?;
        let hash = self.hash_new(password).await?;
        let id = crypto::new_id("prn")?;
        let now = self.now();
        self.tx(move |c| {
            let count: i64 =
                c.query_row("SELECT count(*) FROM identity_principals", [], |r| r.get(0))?;
            if count != 0 {
                return Err(IdentityError::Conflict.into());
            }
            insert_principal(c, &id, &username, PrincipalRole::Owner, &hash, now)?;
            Ok(id)
        })
        .await
    }

    /// The owner creates an additional principal with its own isolated data.
    pub async fn create_principal(
        &self,
        owner: &AuthenticatedSession,
        username: &str,
        password: Password,
    ) -> Result<String> {
        let username = normalize_username(username).ok_or(IdentityError::Invalid("username"))?;
        self.check_policy(&password)?;
        let hash = self.hash_new(password).await?;
        let id = crypto::new_id("prn")?;
        let now = self.now();
        let owner = owner.clone();
        self.tx(move |c| {
            live(c, &owner, now)?;
            if owner.role != PrincipalRole::Owner {
                return Err(IdentityError::Forbidden.into());
            }
            insert_principal(c, &id, &username, PrincipalRole::User, &hash, now)?;
            Ok(id)
        })
        .await
    }

    /// Disables another principal and revokes all of its sessions.
    pub async fn disable_principal(
        &self,
        owner: &AuthenticatedSession,
        principal_id: &str,
    ) -> Result<()> {
        let (owner, target, now) = (owner.clone(), principal_id.to_owned(), self.now());
        self.tx(move |c| {
            live(c, &owner, now)?;
            if owner.role != PrincipalRole::Owner {
                return Err(IdentityError::Forbidden.into());
            }
            if target == owner.principal_id {
                return Err(IdentityError::Invalid("the owner cannot disable itself").into());
            }
            if c.execute(
                "UPDATE identity_principals SET disabled_at=?1 WHERE id=?2 AND disabled_at IS NULL",
                params![now, target],
            )? != 1
            {
                return Err(IdentityError::NotFound.into());
            }
            revoke_sessions_of_principal(c, &target, now, "principal_disabled")?;
            Ok(())
        })
        .await
    }

    /// Rotates the caller's password after verifying the current one and revokes
    /// every other session of the principal.
    pub async fn change_password(
        &self,
        session: &AuthenticatedSession,
        current: Password,
        new: Password,
    ) -> Result<usize> {
        self.check_policy(&new)?;
        let username: String = {
            let s = session.clone();
            let now = self.now();
            self.tx(move |c| {
                live(c, &s, now)?;
                Ok(c.query_row(
                    "SELECT username FROM identity_principals WHERE id=?1",
                    [&s.principal_id],
                    |r| r.get(0),
                )?)
            })
            .await?
        };
        let reserved = self.reserve_attempt(username.clone(), None).await?;
        let verified = self.verify_reserved(&reserved, current).await?;
        let hash = self.hash_new(new).await?;
        let (s, now) = (session.clone(), self.now());
        self.tx(move |c| {
            live(c, &s, now)?;
            let principal = Self::accept_password(c, &username, &verified, now)?;
            if principal != s.principal_id {
                return Err(IdentityError::Unauthenticated.into());
            }
            let generation: i64 = c.query_row(
                "SELECT generation FROM identity_passwords WHERE principal_id=?1",
                [&principal],
                |r| r.get(0),
            )?;
            store_password(c, &principal, &hash, generation + 1, now)?;
            let revoked = c.execute(
                "UPDATE identity_sessions SET revoked_at=?1,revoke_reason='password_changed'
                 WHERE principal_id=?2 AND id<>?3 AND revoked_at IS NULL",
                params![now, principal, s.session_id],
            )?;
            c.execute(
                "DELETE FROM identity_csrf WHERE session_id IN
                 (SELECT id FROM identity_sessions WHERE principal_id=?1 AND id<>?2)",
                params![principal, s.session_id],
            )?;
            Ok(revoked)
        })
        .await
    }

    /// Enrolls a device key over the host's local channel after a password check.
    pub async fn enroll_local_device(
        &self,
        _local: &LocalChannel,
        username: &str,
        password: Password,
        device_name: &str,
        public_key: [u8; 32],
    ) -> Result<EnrolledDevice> {
        let username = normalize_username(username).ok_or(IdentityError::Unauthenticated)?;
        if !valid_label(device_name) || !valid_public_key(&public_key) {
            return Err(IdentityError::Invalid("device"));
        }
        let reserved = self.reserve_attempt(username.clone(), None).await?;
        let verified = self.verify_reserved(&reserved, password).await?;
        let (name, now, id) = (device_name.to_owned(), self.now(), crypto::new_id("dev")?);
        self.tx(move |c| {
            let principal = Self::accept_password(c, &username, &verified, now)?;
            insert_device(c, &id, &principal, &name, &public_key, "local", now)?;
            Ok(EnrolledDevice {
                device_id: id,
                principal_id: principal,
            })
        })
        .await
    }

    /// Issues a single-use nonce the device must sign to open a session.
    pub async fn begin_device_challenge(&self, device_id: &str) -> Result<DeviceChallenge> {
        let (device, now) = (device_id.to_owned(), self.now());
        let (ttl, max) = (
            self.config.challenge_ttl_seconds,
            i64::from(self.config.max_open_challenges_per_device),
        );
        let id = crypto::new_id("chl")?;
        let nonce = crypto::random::<32>()?;
        self.tx(move |c| {
            let active: bool = c
                .query_row(
                    "SELECT 1 FROM identity_devices d JOIN identity_principals p ON p.id=d.principal_id
                     WHERE d.id=?1 AND d.revoked_at IS NULL AND p.disabled_at IS NULL",
                    [&device],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if !active {
                return Err(IdentityError::Unauthenticated.into());
            }
            c.execute(
                "DELETE FROM identity_challenges WHERE device_id=?1 AND (consumed_at IS NOT NULL OR expires_at<=?2)",
                params![device, now],
            )?;
            let (open, soonest): (i64, Option<i64>) = c.query_row(
                "SELECT count(*),min(expires_at) FROM identity_challenges WHERE device_id=?1",
                [&device],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if open >= max {
                return Err(IdentityError::RateLimited {
                    retry_after_seconds: soonest.unwrap_or(now + ttl) - now,
                }
                .into());
            }
            c.execute(
                "INSERT INTO identity_challenges(id,device_id,nonce,expires_at) VALUES(?1,?2,?3,?4)",
                params![id, device, &nonce[..], now + ttl],
            )?;
            Ok(DeviceChallenge {
                challenge_id: id,
                nonce,
                expires_at: now + ttl,
            })
        })
        .await
    }

    /// Password plus device-key proof; opens a device-bound session and returns
    /// its bearer token and a first CSRF token.
    pub async fn login(&self, request: LoginRequest) -> Result<IssuedSession> {
        let LoginRequest {
            username,
            password,
            device,
        } = request;
        let username = normalize_username(&username).ok_or(IdentityError::Unauthenticated)?;
        let device_id = device.device_id.clone();
        let reserved = self.reserve_attempt(username.clone(), Some(device)).await?;
        let device_ok = reserved.device_ok;
        let verified = self.verify_reserved(&reserved, password).await?;
        let session = crypto::mint_token(SESSION_KIND, "ses")?;
        let csrf = crypto::mint_token(CSRF_KIND, "csr")?;
        let config = self.config.clone();
        let now = self.now();
        self.tx(move |c| {
            if !device_ok {
                return Err(IdentityError::Unauthenticated.into());
            }
            let principal = Self::accept_password(c, &username, &verified, now)?;
            let device_live: bool = c
                .query_row(
                    "SELECT 1 FROM identity_devices WHERE id=?1 AND principal_id=?2 AND revoked_at IS NULL",
                    params![device_id, principal],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if !device_live {
                return Err(IdentityError::Unauthenticated.into());
            }
            let role: String = c.query_row(
                "SELECT role FROM identity_principals WHERE id=?1",
                [&principal],
                |r| r.get(0),
            )?;
            c.execute(
                "DELETE FROM identity_sessions WHERE principal_id=?1 AND (expires_at<=?2 OR idle_expires_at<=?2)",
                params![principal, now],
            )?;
            let expires = now.saturating_add(config.session_ttl_seconds);
            let idle = now.saturating_add(config.session_idle_seconds).min(expires);
            c.execute(
                "INSERT INTO identity_sessions(id,principal_id,device_id,secret_hash,created_at,last_seen_at,idle_expires_at,expires_at)
                 VALUES(?1,?2,?3,?4,?5,?5,?6,?7)",
                params![session.id, principal, device_id, &session.secret_hash[..], now, idle, expires],
            )?;
            let csrf_expires = now.saturating_add(config.csrf_ttl_seconds).min(expires);
            c.execute(
                "INSERT INTO identity_csrf(id,session_id,secret_hash,created_at,expires_at) VALUES(?1,?2,?3,?4,?5)",
                params![csrf.id, session.id, &csrf.secret_hash[..], now, csrf_expires],
            )?;
            Ok(IssuedSession {
                session: AuthenticatedSession {
                    principal_id: principal,
                    session_id: session.id.clone(),
                    device_id,
                    role: PrincipalRole::parse(&role)?,
                    expires_at: expires,
                },
                token: SessionToken(session.token),
                csrf: CsrfToken(csrf.token),
                idle_expires_at: idle,
            })
        })
        .await
    }

    /// Verifies a bearer token, slides its idle expiry and returns the capability.
    /// Unknown, malformed, expired and revoked tokens are indistinguishable.
    pub async fn authenticate(&self, token: &str) -> Result<AuthenticatedSession> {
        let (id, digest) = crypto::parse_token(SESSION_KIND, "ses", token)
            .ok_or(IdentityError::Unauthenticated)?;
        let (now, idle_ttl) = (self.now(), self.config.session_idle_seconds);
        self.tx(move |c| {
            let row: Option<(String, String, Vec<u8>, i64, String)> = c
                .query_row(
                    "SELECT s.principal_id,s.device_id,s.secret_hash,s.expires_at,p.role
                     FROM identity_sessions s JOIN identity_principals p ON p.id=s.principal_id WHERE s.id=?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;
            let Some((principal_id, device_id, stored, expires_at, role)) = row else {
                return Err(IdentityError::Unauthenticated.into());
            };
            if !crypto::ct_eq(&stored, &digest) {
                return Err(IdentityError::Unauthenticated.into());
            }
            let session = AuthenticatedSession {
                principal_id,
                session_id: id,
                device_id,
                role: PrincipalRole::parse(&role)?,
                expires_at,
            };
            live(c, &session, now)?;
            c.execute(
                "UPDATE identity_sessions SET last_seen_at=?1,idle_expires_at=min(?2,expires_at) WHERE id=?3",
                params![now, now.saturating_add(idle_ttl), session.session_id],
            )?;
            Ok(session)
        })
        .await
    }

    pub async fn list_sessions(&self, session: &AuthenticatedSession) -> Result<Vec<SessionInfo>> {
        let (s, now) = (session.clone(), self.now());
        self.tx(move |c| {
            live(c, &s, now)?;
            let mut statement = c.prepare(
                "SELECT id,device_id,created_at,last_seen_at,expires_at FROM identity_sessions
                 WHERE principal_id=?1 AND revoked_at IS NULL AND expires_at>?2 AND idle_expires_at>?2
                 ORDER BY created_at,id",
            )?;
            let rows = statement
                .query_map(params![s.principal_id, now], |r| {
                    let id: String = r.get(0)?;
                    Ok(SessionInfo {
                        current: id == s.session_id,
                        session_id: id,
                        device_id: r.get(1)?,
                        created_at: r.get(2)?,
                        last_seen_at: r.get(3)?,
                        expires_at: r.get(4)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    pub async fn list_devices(&self, session: &AuthenticatedSession) -> Result<Vec<DeviceInfo>> {
        let (s, now) = (session.clone(), self.now());
        self.tx(move |c| {
            live(c, &s, now)?;
            let mut statement = c.prepare(
                "SELECT id,name,enrolled_via,created_at,revoked_at IS NOT NULL FROM identity_devices
                 WHERE principal_id=?1 ORDER BY created_at,id",
            )?;
            let rows = statement
                .query_map([&s.principal_id], |r| {
                    Ok(DeviceInfo {
                        device_id: r.get(0)?,
                        name: r.get(1)?,
                        enrolled_via: r.get(2)?,
                        created_at: r.get(3)?,
                        revoked: r.get(4)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Revokes one of the caller's own sessions. Other principals' sessions
    /// are reported as not found.
    pub async fn revoke_session(&self, session: &AuthenticatedSession, target: &str) -> Result<()> {
        let (s, target, now) = (session.clone(), target.to_owned(), self.now());
        self.tx(move |c| {
            live(c, &s, now)?;
            if c.execute(
                "UPDATE identity_sessions SET revoked_at=?1,revoke_reason='revoked'
                 WHERE id=?2 AND principal_id=?3 AND revoked_at IS NULL",
                params![now, target, s.principal_id],
            )? != 1
            {
                return Err(IdentityError::NotFound.into());
            }
            c.execute("DELETE FROM identity_csrf WHERE session_id=?1", [&target])?;
            Ok(())
        })
        .await
    }

    pub async fn logout(&self, session: &AuthenticatedSession) -> Result<()> {
        self.revoke_session(session, &session.session_id.clone())
            .await
    }

    pub async fn revoke_other_sessions(&self, session: &AuthenticatedSession) -> Result<usize> {
        let (s, now) = (session.clone(), self.now());
        self.tx(move |c| {
            live(c, &s, now)?;
            let changed = c.execute(
                "UPDATE identity_sessions SET revoked_at=?1,revoke_reason='revoked'
                 WHERE principal_id=?2 AND id<>?3 AND revoked_at IS NULL",
                params![now, s.principal_id, s.session_id],
            )?;
            c.execute(
                "DELETE FROM identity_csrf WHERE session_id IN
                 (SELECT id FROM identity_sessions WHERE principal_id=?1 AND id<>?2)",
                params![s.principal_id, s.session_id],
            )?;
            Ok(changed)
        })
        .await
    }

    /// Revokes one of the caller's devices, its sessions and open challenges.
    pub async fn revoke_device(
        &self,
        session: &AuthenticatedSession,
        device_id: &str,
    ) -> Result<usize> {
        let (s, device, now) = (session.clone(), device_id.to_owned(), self.now());
        self.tx(move |c| {
            live(c, &s, now)?;
            if c.execute(
                "UPDATE identity_devices SET revoked_at=?1 WHERE id=?2 AND principal_id=?3 AND revoked_at IS NULL",
                params![now, device, s.principal_id],
            )? != 1
            {
                return Err(IdentityError::NotFound.into());
            }
            c.execute("DELETE FROM identity_challenges WHERE device_id=?1", [&device])?;
            revoke_sessions(c, SessionsOf::Device, &device, now, "device_revoked")
        })
        .await
    }

    /// Issues an additional CSRF token for the session (older ones beyond the
    /// per-session cap are discarded).
    pub async fn issue_csrf(&self, session: &AuthenticatedSession) -> Result<CsrfToken> {
        let (s, now) = (session.clone(), self.now());
        let (ttl, max) = (
            self.config.csrf_ttl_seconds,
            i64::from(self.config.max_csrf_per_session),
        );
        let minted = crypto::mint_token(CSRF_KIND, "csr")?;
        self.tx(move |c| {
            live(c, &s, now)?;
            c.execute(
                "DELETE FROM identity_csrf WHERE session_id=?1 AND (expires_at<=?2 OR id IN
                   (SELECT id FROM identity_csrf WHERE session_id=?1 ORDER BY created_at DESC,id DESC LIMIT -1 OFFSET ?3))",
                params![s.session_id, now, max - 1],
            )?;
            c.execute(
                "INSERT INTO identity_csrf(id,session_id,secret_hash,created_at,expires_at) VALUES(?1,?2,?3,?4,?5)",
                params![minted.id, s.session_id, &minted.secret_hash[..], now, now.saturating_add(ttl).min(s.expires_at)],
            )?;
            Ok(CsrfToken(minted.token))
        })
        .await
    }

    /// Verifies a CSRF token for a state-changing request of this live session.
    /// A token issued to another session never verifies.
    pub async fn verify_csrf(&self, session: &AuthenticatedSession, token: &str) -> Result<()> {
        let (id, digest) =
            crypto::parse_token(CSRF_KIND, "csr", token).ok_or(IdentityError::Unauthenticated)?;
        let (s, now) = (session.clone(), self.now());
        self.read(move |c| {
            live(c, &s, now)?;
            let stored: Option<(String, Vec<u8>, i64)> = c
                .query_row(
                    "SELECT session_id,secret_hash,expires_at FROM identity_csrf WHERE id=?1",
                    [&id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            match stored {
                Some((session_id, hash, expires))
                    if crypto::ct_eq(session_id.as_bytes(), s.session_id.as_bytes())
                        && crypto::ct_eq(&hash, &digest)
                        && now < expires =>
                {
                    Ok(())
                }
                _ => Err(IdentityError::Unauthenticated.into()),
            }
        })
        .await
    }

    /// Creates a short-lived single-use code that lets a remote device enroll
    /// into the caller's principal.
    pub async fn create_pairing_code(&self, session: &AuthenticatedSession) -> Result<PairingCode> {
        let (s, now) = (session.clone(), self.now());
        let (ttl, max) = (
            self.config.pairing_ttl_seconds,
            i64::from(self.config.max_active_pairing_codes),
        );
        let code = crypto::new_code()?;
        let hash = crypto::code_hash("pairing", &code);
        let id = crypto::new_id("pcd")?;
        let expires = now + ttl;
        self.tx(move |c| {
            live(c, &s, now)?;
            c.execute(
                "DELETE FROM identity_codes WHERE issuer_id=?1 AND purpose='pairing' AND (expires_at<=?2 OR consumed_at IS NOT NULL)",
                params![s.principal_id, now],
            )?;
            let (open, soonest): (i64, Option<i64>) = c.query_row(
                "SELECT count(*),min(expires_at) FROM identity_codes WHERE issuer_id=?1 AND purpose='pairing'",
                [&s.principal_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if open >= max {
                return Err(IdentityError::RateLimited {
                    retry_after_seconds: soonest.unwrap_or(expires) - now,
                }
                .into());
            }
            c.execute(
                "INSERT INTO identity_codes(id,purpose,code_hash,issuer_id,created_at,expires_at) VALUES(?1,'pairing',?2,?3,?4,?5)",
                params![id, &hash[..], s.principal_id, now, expires],
            )?;
            Ok(())
        })
        .await?;
        let display = Zeroizing::new(format!("{}-{}", &code[..5], &code[5..]));
        Ok(PairingCode {
            code: PairingCodeSecret(display),
            expires_at: expires,
        })
    }

    /// Redeems a pairing code from a remote device. `source_key` is the host's
    /// identifier for the requesting peer (e.g. its address); failures are
    /// rate-limited per source and globally, and limits also block correct codes.
    pub async fn redeem_pairing_code(
        &self,
        source_key: &str,
        code: &str,
        device_name: &str,
        public_key: [u8; 32],
    ) -> Result<EnrolledDevice> {
        if !valid_label(device_name) || !valid_public_key(&public_key) {
            return Err(IdentityError::Invalid("device"));
        }
        let name = device_name.to_owned();
        let id = crypto::new_id("dev")?;
        self.redeem_code("pairing", source_key, code, move |c, code, now| {
            insert_device(c, &id, &code.issuer_id, &name, &public_key, "pairing", now)?;
            Ok(EnrolledDevice {
                device_id: id,
                principal_id: code.issuer_id.clone(),
            })
        })
        .await
    }

    /// Shared single-use code redemption. The lookup, rate accounting, CAS
    /// consumption and the purpose-specific effect run in one transaction.
    pub(crate) async fn redeem_code<T: Send + 'static>(
        &self,
        purpose: &'static str,
        source_key: &str,
        code: &str,
        effect: impl FnOnce(&Connection, &RedeemedCode, i64) -> Tx<T> + Send + 'static,
    ) -> Result<T> {
        if source_key.is_empty() || source_key.len() > 256 {
            return Err(IdentityError::Invalid("source key"));
        }
        let source = crypto::hex(&crypto::sha256(&[b"source", source_key.as_bytes()]));
        let normalized = crypto::normalize_code(code);
        let (now, source_limit, global_limit) = (
            self.now(),
            self.config.code_source_limit,
            self.config.code_global_limit,
        );
        self.tx(move |c| {
            let blocked = [
                rate_blocked(c, CODE_SOURCE_SCOPE, &source, source_limit, now)?,
                rate_blocked(c, CODE_GLOBAL_SCOPE, purpose, global_limit, now)?,
            ];
            if let Some(retry) = blocked.into_iter().flatten().max() {
                return Ok(Err(IdentityError::RateLimited {
                    retry_after_seconds: retry,
                }));
            }
            let fail = |c: &Connection| -> Tx<std::result::Result<T, IdentityError>> {
                rate_fail(c, CODE_SOURCE_SCOPE, &source, source_limit, now)?;
                rate_fail(c, CODE_GLOBAL_SCOPE, purpose, global_limit, now)?;
                Ok(Err(IdentityError::Unauthenticated))
            };
            let Some(normalized) = normalized else {
                return fail(c);
            };
            let hash = crypto::code_hash(purpose, &normalized);
            let row: Option<CodeRow> = c
                .query_row(
                    "SELECT k.id,k.code_hash,k.issuer_id,k.team_id,k.role FROM identity_codes k
                     JOIN identity_principals p ON p.id=k.issuer_id
                     WHERE k.code_hash=?1 AND k.purpose=?2 AND k.consumed_at IS NULL
                       AND k.expires_at>?3 AND p.disabled_at IS NULL",
                    params![&hash[..], purpose, now],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;
            let Some((id, stored, issuer_id, team_id, role)) = row else {
                return fail(c);
            };
            if !crypto::ct_eq(&stored, &hash) {
                return fail(c);
            }
            if c.execute(
                "UPDATE identity_codes SET consumed_at=?1 WHERE id=?2 AND consumed_at IS NULL",
                params![now, id],
            )? != 1
            {
                return fail(c);
            }
            let redeemed = RedeemedCode {
                id: id.clone(),
                issuer_id,
                team_id,
                role,
            };
            let value = effect(c, &redeemed, now)?;
            Ok(Ok(value))
        })
        .await?
    }
}

/// One pending-code lookup row: id, code hash, issuer, team, role.
type CodeRow = (String, Vec<u8>, String, Option<String>, Option<String>);

pub(crate) struct RedeemedCode {
    #[allow(dead_code)]
    pub id: String,
    pub issuer_id: String,
    pub team_id: Option<String>,
    pub role: Option<String>,
}

fn insert_principal(
    c: &Connection,
    id: &str,
    username: &str,
    role: PrincipalRole,
    hash: &NewHash,
    now: i64,
) -> Tx<()> {
    let taken: bool = c
        .query_row(
            "SELECT 1 FROM identity_principals WHERE username=?1",
            [username],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if taken {
        return Err(IdentityError::Conflict.into());
    }
    let role = match role {
        PrincipalRole::Owner => "owner",
        PrincipalRole::User => "user",
    };
    c.execute(
        "INSERT INTO identity_principals(id,username,role,created_at) VALUES(?1,?2,?3,?4)",
        params![id, username, role, now],
    )?;
    store_password(c, id, hash, 1, now)
}

fn insert_device(
    c: &Connection,
    id: &str,
    principal_id: &str,
    name: &str,
    public_key: &[u8; 32],
    via: &str,
    now: i64,
) -> Tx<()> {
    let duplicate: bool = c
        .query_row(
            "SELECT 1 FROM identity_devices WHERE public_key=?1",
            [&public_key[..]],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if duplicate {
        return Err(Fail::Domain(IdentityError::Conflict));
    }
    c.execute(
        "INSERT INTO identity_devices(id,principal_id,name,public_key,enrolled_via,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
        params![id, principal_id, name, &public_key[..], via, now],
    )?;
    Ok(())
}
