//! Shared-core identity library: principals with argon2id passwords, devices bound
//! to Ed25519 keys, device-bound sessions with expiry and revocation, single-use
//! rate-limited remote pairing codes, CSRF tokens, and teams with member roles,
//! shared resources and quotas.
//!
//! Every per-principal operation takes an [`AuthenticatedSession`], which can
//! only be produced by this crate after verifying a live session token, and is
//! re-validated inside the same storage transaction that reads or changes data.
//! Bearer secrets are minted from operating-system randomness, stored only as
//! SHA-256 digests and compared in constant time. Secret wrapper types redact
//! `Debug`, implement neither `Display` nor `Serialize`, and zeroize on drop;
//! error messages are fixed strings that never include caller input.
//!
//! This is a library only: gateway/HTTP wiring, `Origin` checks, TLS and any UI
//! are the host's responsibility and are not implemented here.

mod accounts;
mod crypto;
mod teams;

pub use accounts::{
    DeviceChallenge, DeviceInfo, DeviceProof, EnrolledDevice, IssuedSession, LoginRequest,
    PairingCode, SessionInfo,
};
pub use crypto::device_proof_message;
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane, rusqlite};
use rusqlite::OptionalExtension;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI64, Ordering},
};
pub use teams::{
    Admission, AdmissionReceipt, HostProxyTicket, HostShareView, HostTeamView, MemberQuotaView,
    MemberView, ProjectPermission, QuotaSpec, ResourceKind, SharedResourceView, TeamInvite,
    TeamRole, TeamView,
};
use zeroize::Zeroizing;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// Wrong password, unknown user, bad/expired/revoked token, failed device
    /// proof or unknown code. Deliberately undifferentiated.
    #[error("authentication failed")]
    Unauthenticated,
    #[error("too many failed attempts; retry later")]
    RateLimited { retry_after_seconds: i64 },
    /// Also returned for objects owned by another principal, so existence is not revealed.
    #[error("not found")]
    NotFound,
    #[error("operation not permitted for this principal")]
    Forbidden,
    #[error("conflicting identity state")]
    Conflict,
    #[error("password does not satisfy the policy")]
    WeakPassword,
    #[error("resource is not shared with this team")]
    NotShared,
    #[error("no quota is configured for this resource")]
    QuotaNotConfigured,
    #[error("quota exceeded")]
    QuotaExceeded { remaining: u64, window_ends_at: i64 },
    #[error("invalid argument: {0}")]
    Invalid(&'static str),
    #[error("operating-system randomness unavailable")]
    Randomness,
    #[error("host credential unavailable")]
    Vault,
    #[error("identity storage failed")]
    Storage(#[source] excalibur_store::Error),
}
pub type Result<T> = std::result::Result<T, IdentityError>;

/// Failure inside a write transaction: always rolls the job back.
pub(crate) enum Fail {
    Domain(IdentityError),
    Store(excalibur_store::Error),
}
impl From<IdentityError> for Fail {
    fn from(e: IdentityError) -> Self {
        Self::Domain(e)
    }
}
impl From<rusqlite::Error> for Fail {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(e.into())
    }
}
pub(crate) type Tx<T> = std::result::Result<T, Fail>;

/// Injected time source (Unix seconds) so expiry is testable deterministically.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> i64;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> i64 {
        excalibur_store::unix_seconds()
    }
}
/// Manually advanced clock for deterministic tests and simulations.
#[derive(Clone, Default)]
pub struct ManualClock(Arc<AtomicI64>);
impl ManualClock {
    pub fn new(now: i64) -> Self {
        Self(Arc::new(AtomicI64::new(now)))
    }
    pub fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
    pub fn set(&self, now: i64) {
        self.0.store(now, Ordering::SeqCst);
    }
}
impl Clock for ManualClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Argon2id cost and password-length policy. The default follows the OWASP
/// password storage recommendation (m=19 MiB, t=2, p=1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasswordPolicy {
    pub min_chars: usize,
    pub max_bytes: usize,
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}
impl Default for PasswordPolicy {
    fn default() -> Self {
        Self {
            min_chars: 12,
            max_bytes: 1024,
            memory_kib: 19 * 1024,
            iterations: 2,
            parallelism: 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimit {
    pub max_failures: u32,
    pub window_seconds: i64,
}

#[derive(Clone, Debug)]
pub struct IdentityConfig {
    pub password: PasswordPolicy,
    /// Absolute session lifetime.
    pub session_ttl_seconds: i64,
    /// Sliding inactivity limit; never extends beyond the absolute lifetime.
    pub session_idle_seconds: i64,
    pub csrf_ttl_seconds: i64,
    pub challenge_ttl_seconds: i64,
    pub pairing_ttl_seconds: i64,
    pub invite_ttl_seconds: i64,
    pub max_active_pairing_codes: u32,
    pub max_csrf_per_session: u32,
    pub max_open_challenges_per_device: u32,
    /// Failed password attempts per username.
    pub login_limit: RateLimit,
    /// Failed pairing/invite redemptions per host-provided source key.
    pub code_source_limit: RateLimit,
    /// Failed pairing/invite redemptions across all sources.
    pub code_global_limit: RateLimit,
}
impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            password: PasswordPolicy::default(),
            session_ttl_seconds: 30 * 86_400,
            session_idle_seconds: 7 * 86_400,
            csrf_ttl_seconds: 12 * 3_600,
            challenge_ttl_seconds: 120,
            pairing_ttl_seconds: 300,
            invite_ttl_seconds: 86_400,
            max_active_pairing_codes: 3,
            max_csrf_per_session: 16,
            max_open_challenges_per_device: 8,
            login_limit: RateLimit {
                max_failures: 5,
                window_seconds: 900,
            },
            code_source_limit: RateLimit {
                max_failures: 5,
                window_seconds: 60,
            },
            code_global_limit: RateLimit {
                max_failures: 20,
                window_seconds: 60,
            },
        }
    }
}
impl IdentityConfig {
    fn validate(&self) -> Result<()> {
        let p = &self.password;
        let limits = [
            self.login_limit,
            self.code_source_limit,
            self.code_global_limit,
        ];
        if p.min_chars == 0
            || p.max_bytes < p.min_chars
            || p.max_bytes > 4096
            || p.iterations == 0
            || p.parallelism == 0
            || p.memory_kib < 8 * p.parallelism
            || self.session_ttl_seconds <= 0
            || self.session_idle_seconds <= 0
            || self.csrf_ttl_seconds <= 0
            || !(1..=600).contains(&self.challenge_ttl_seconds)
            || !(1..=900).contains(&self.pairing_ttl_seconds)
            || !(1..=7 * 86_400).contains(&self.invite_ttl_seconds)
            || self.max_active_pairing_codes == 0
            || self.max_csrf_per_session == 0
            || self.max_open_challenges_per_device == 0
            || limits
                .iter()
                .any(|l| l.max_failures == 0 || l.window_seconds <= 0)
        {
            return Err(IdentityError::Invalid("identity configuration"));
        }
        Ok(())
    }
}

macro_rules! secret_string {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        pub struct $name(Zeroizing<String>);
        impl $name {
            /// Trusted boundary: hand the value to the secure client channel only.
            pub fn expose(&self) -> &str {
                &self.0
            }
        }
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(concat!(stringify!($name), "([REDACTED])"))
            }
        }
    };
}
secret_string!(
    /// A password supplied through the client's secure input channel.
    Password
);
impl Password {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }
}
secret_string!(
    /// Session bearer token. Never place it in a URL.
    SessionToken
);
secret_string!(
    /// Anti-CSRF synchronizer token bound to one session.
    CsrfToken
);
secret_string!(
    /// Short-lived single-use remote pairing code shown on an already-paired device.
    PairingCodeSecret
);
secret_string!(
    /// Short-lived single-use team invitation code shown to an entitled inviter.
    TeamInviteCode
);

/// Host-constructed capability asserting the request arrived over the host's
/// authenticated local channel (loopback plus local token). Remote callers must
/// enroll through a pairing code instead.
#[derive(Debug)]
pub struct LocalChannel(());
impl LocalChannel {
    pub fn host_attested() -> Self {
        Self(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalRole {
    Owner,
    User,
}
impl PrincipalRole {
    fn parse(s: &str) -> Tx<Self> {
        match s {
            "owner" => Ok(Self::Owner),
            "user" => Ok(Self::User),
            _ => Err(Fail::Store(excalibur_store::Error::Invalid(
                "stored principal role",
            ))),
        }
    }
}

/// Verified, live session capability. Only this crate constructs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedSession {
    principal_id: String,
    session_id: String,
    device_id: String,
    role: PrincipalRole,
    expires_at: i64,
}
impl AuthenticatedSession {
    pub fn principal_id(&self) -> &str {
        &self.principal_id
    }
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn device_id(&self) -> &str {
        &self.device_id
    }
    pub fn role(&self) -> PrincipalRole {
        self.role
    }
    pub fn expires_at(&self) -> i64 {
        self.expires_at
    }
    /// Opaque per-principal storage namespace (data directory name, memory and
    /// master scope). Distinct principals always receive distinct namespaces.
    pub fn data_namespace(&self) -> String {
        data_namespace(&self.principal_id)
    }
}
pub(crate) fn data_namespace(principal_id: &str) -> String {
    format!(
        "principal-{}",
        crypto::hex(
            &crypto::sha256(&[b"excalibur-identity/v1/namespace", principal_id.as_bytes()])[..16]
        )
    )
}

#[derive(Clone)]
pub struct Identity {
    db: Database,
    config: Arc<IdentityConfig>,
    clock: Arc<dyn Clock>,
    /// Fixed per-process salt for the dummy hash used when the username is unknown.
    dummy_salt: [u8; crypto::SALT_LEN],
}
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Identity")
    }
}

const SCHEMAS: [(i64, &str); 1] = [(1, include_str!("schema1.sql"))];

impl Identity {
    /// Opens the identity tables in a host-level `Main` database, applying the
    /// additive, checksummed schema. A modified historical schema is refused.
    pub async fn open(db: Database, config: IdentityConfig, clock: Arc<dyn Clock>) -> Result<Self> {
        config.validate()?;
        if db.kind() != DatabaseKind::Main {
            return Err(IdentityError::Invalid("identity requires a main database"));
        }
        db.write(Priority::Journal, |c| {
            c.execute_batch(
                "CREATE TABLE IF NOT EXISTS identity_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL)",
            )?;
            for (version, schema) in SCHEMAS {
                let checksum = format!("{:x}", Sha256::digest(schema.as_bytes()));
                let known: Option<String> = c
                    .query_row(
                        "SELECT checksum FROM identity_schema WHERE version=?1",
                        [version],
                        |r| r.get(0),
                    )
                    .optional()?;
                if known.as_ref().is_some_and(|stored| stored != &checksum) {
                    return Err(excalibur_store::Error::Invalid(
                        "identity schema checksum mismatch",
                    ));
                }
                c.execute_batch(schema)?;
                c.execute(
                    "INSERT OR IGNORE INTO identity_schema(version,checksum) VALUES(?1,?2)",
                    rusqlite::params![version, checksum],
                )?;
            }
            Ok(())
        })
        .await
        .map_err(IdentityError::Storage)?;
        Ok(Self {
            db,
            config: Arc::new(config),
            clock,
            dummy_salt: crypto::random()?,
        })
    }

    pub fn config(&self) -> &IdentityConfig {
        &self.config
    }

    pub(crate) fn now(&self) -> i64 {
        self.clock.now()
    }

    /// Runs `work` in one storage transaction. Any `Fail` rolls the job back;
    /// outcomes that must persist despite rejecting the caller (rate-limit
    /// counters, consumed challenges) return `Ok(Err(..))` instead.
    pub(crate) async fn tx<T: Send + 'static>(
        &self,
        work: impl FnOnce(&rusqlite::Connection) -> Tx<T> + Send + 'static,
    ) -> Result<T> {
        let domain: Arc<Mutex<Option<IdentityError>>> = Arc::default();
        let slot = domain.clone();
        let result = self
            .db
            .write(Priority::Interactive, move |c| match work(c) {
                Ok(value) => Ok(value),
                Err(Fail::Store(e)) => Err(e),
                Err(Fail::Domain(e)) => {
                    *slot.lock().unwrap() = Some(e);
                    Err(excalibur_store::Error::Invalid(
                        "identity operation rejected",
                    ))
                }
            })
            .await;
        match result {
            Ok(value) => Ok(value),
            Err(e) => Err(domain
                .lock()
                .unwrap()
                .take()
                .unwrap_or(IdentityError::Storage(e))),
        }
    }

    pub(crate) async fn read<T: Send + 'static>(
        &self,
        work: impl FnOnce(&rusqlite::Connection) -> Tx<T> + Send + 'static,
    ) -> Result<T> {
        let domain: Arc<Mutex<Option<IdentityError>>> = Arc::default();
        let slot = domain.clone();
        let result = self
            .db
            .read(ReadLane::Interactive, move |c| match work(c) {
                Ok(value) => Ok(value),
                Err(Fail::Store(e)) => Err(e),
                Err(Fail::Domain(e)) => {
                    *slot.lock().unwrap() = Some(e);
                    Err(excalibur_store::Error::Invalid(
                        "identity operation rejected",
                    ))
                }
            })
            .await;
        match result {
            Ok(value) => Ok(value),
            Err(e) => Err(domain
                .lock()
                .unwrap()
                .take()
                .unwrap_or(IdentityError::Storage(e))),
        }
    }
}

/// Returns the remaining lockout in seconds when `key` has exhausted `limit`.
pub(crate) fn rate_blocked(
    c: &rusqlite::Connection,
    scope: &str,
    key: &str,
    limit: RateLimit,
    now: i64,
) -> Tx<Option<i64>> {
    let row: Option<(i64, i64)> = c
        .query_row(
            "SELECT window_start,failures FROM identity_rate WHERE scope=?1 AND key=?2",
            [scope, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match row {
        Some((start, failures))
            if now < start.saturating_add(limit.window_seconds)
                && now >= start
                && failures >= i64::from(limit.max_failures) =>
        {
            Some(start.saturating_add(limit.window_seconds) - now)
        }
        _ => None,
    })
}

pub(crate) fn rate_fail(
    c: &rusqlite::Connection,
    scope: &str,
    key: &str,
    limit: RateLimit,
    now: i64,
) -> Tx<()> {
    c.execute(
        "INSERT INTO identity_rate(scope,key,window_start,failures) VALUES(?1,?2,?3,1)
         ON CONFLICT(scope,key) DO UPDATE SET
           failures=CASE WHEN ?3>=window_start AND ?3<window_start+?4 THEN failures+1 ELSE 1 END,
           window_start=CASE WHEN ?3>=window_start AND ?3<window_start+?4 THEN window_start ELSE ?3 END",
        rusqlite::params![scope, key, now, limit.window_seconds],
    )?;
    Ok(())
}

pub(crate) fn rate_clear(c: &rusqlite::Connection, scope: &str, key: &str) -> Tx<()> {
    c.execute(
        "DELETE FROM identity_rate WHERE scope=?1 AND key=?2",
        [scope, key],
    )?;
    Ok(())
}
