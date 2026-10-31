//! Teams: the host (leader) shares providers and tools with members under
//! per-member quotas. Members never receive host credentials or references to
//! them; the host's gateway resolves a [`HostProxyTicket`] against its own vault
//! and signs the upstream request itself.
use crate::accounts::{live, valid_label};
use crate::crypto;
use crate::{AuthenticatedSession, Identity, IdentityError, Result, TeamInviteCode, Tx};
use excalibur_store::rusqlite::{Connection, OptionalExtension, params};
use excalibur_vault::{SecretRef, Vault};
use serde::Serialize;
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamRole {
    Host,
    Admin,
    Member,
}
impl TeamRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "host" => Some(Self::Host),
            "admin" => Some(Self::Admin),
            "member" => Some(Self::Member),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Provider,
    Tool,
}
impl ResourceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Tool => "tool",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "provider" => Some(Self::Provider),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }
}

/// `Write` implies `Read`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectPermission {
    Read,
    Write,
}
impl ProjectPermission {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// A quota for one shared resource. `member_id: None` is the team default,
/// applied to each member separately; a member-specific row overrides it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QuotaSpec {
    pub member_id: Option<String>,
    pub kind: ResourceKind,
    pub name: String,
    pub limit_units: u64,
    pub window_seconds: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MemberView {
    pub principal_id: String,
    pub username: String,
    pub role: TeamRole,
}

/// What a member may know about a shared resource: its kind and name only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SharedResourceView {
    pub kind: ResourceKind,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MemberQuotaView {
    pub kind: ResourceKind,
    pub name: String,
    pub limit_units: u64,
    pub window_seconds: i64,
    pub used_units: u64,
    pub window_ends_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TeamView {
    pub team_id: String,
    pub name: String,
    pub my_role: TeamRole,
    pub members: Vec<MemberView>,
    pub shares: Vec<SharedResourceView>,
    pub my_quotas: Vec<MemberQuotaView>,
}

/// Host-only share listing. Even here only the vault *reference* appears;
/// the secret value stays inside the host vault.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostShareView {
    pub kind: ResourceKind,
    pub name: String,
    pub credential: Option<SecretRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostTeamView {
    pub team: TeamView,
    pub shares: Vec<HostShareView>,
    pub quotas: Vec<QuotaSpec>,
}

#[derive(Debug)]
pub struct TeamInvite {
    pub code: TeamInviteCode,
    pub expires_at: i64,
}

/// Member-safe result of an admission; safe to return to the member's client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AdmissionReceipt {
    pub admission_id: String,
    pub team_id: String,
    pub kind: ResourceKind,
    pub name: String,
    pub reserved_units: u64,
    pub remaining_units: u64,
    pub window_ends_at: i64,
    pub replayed: bool,
}

/// Host-side handle for executing an admitted member request. It is neither
/// `Clone` nor `Serialize`, its `Debug` is redacted, and the only way to reach
/// the credential is [`HostProxyTicket::with_host_credential`] against the
/// host's own vault.
pub struct HostProxyTicket {
    admission_id: String,
    team_id: String,
    member_id: String,
    kind: ResourceKind,
    name: String,
    credential: Option<SecretRef>,
}
impl std::fmt::Debug for HostProxyTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostProxyTicket")
            .field("admission_id", &self.admission_id)
            .field("kind", &self.kind)
            .field("name", &self.name)
            .field("credential", &"[REDACTED]")
            .finish()
    }
}
impl HostProxyTicket {
    pub fn admission_id(&self) -> &str {
        &self.admission_id
    }
    pub fn team_id(&self) -> &str {
        &self.team_id
    }
    pub fn member_id(&self) -> &str {
        &self.member_id
    }
    pub fn kind(&self) -> ResourceKind {
        self.kind
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Trusted host executor boundary: resolves the shared credential in the
    /// host's vault for signing the upstream request. The closure must not
    /// return or publish the borrowed bytes.
    pub fn with_host_credential<T>(
        &self,
        vault: &Vault,
        sign: impl FnOnce(&[u8]) -> T,
    ) -> Result<T> {
        let reference = self
            .credential
            .as_ref()
            .ok_or(IdentityError::Invalid("shared resource has no credential"))?;
        vault
            .with_secret(reference, sign)
            .map_err(|_| IdentityError::Vault)
    }
}

#[derive(Debug)]
pub struct Admission {
    pub receipt: AdmissionReceipt,
    pub ticket: HostProxyTicket,
}

fn valid_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b':'))
}

fn role_in(c: &Connection, team: &str, principal: &str) -> Tx<Option<TeamRole>> {
    let role: Option<String> = c
        .query_row(
            "SELECT role FROM identity_team_members WHERE team_id=?1 AND principal_id=?2",
            [team, principal],
            |r| r.get(0),
        )
        .optional()?;
    Ok(role.as_deref().and_then(TeamRole::parse))
}

/// Membership gate: non-members cannot distinguish a foreign team from none.
fn member_role(c: &Connection, team: &str, s: &AuthenticatedSession) -> Tx<TeamRole> {
    role_in(c, team, &s.principal_id)?.ok_or_else(|| IdentityError::NotFound.into())
}

fn require(role: TeamRole, allowed: &[TeamRole]) -> Tx<()> {
    if allowed.contains(&role) {
        Ok(())
    } else {
        Err(IdentityError::Forbidden.into())
    }
}

fn units(value: u64) -> Tx<i64> {
    i64::try_from(value)
        .ok()
        .filter(|v| *v > 0 && *v <= i64::MAX / 4)
        .ok_or_else(|| IdentityError::Invalid("units").into())
}

struct Quota {
    limit: i64,
    window: i64,
}

fn effective_quota(
    c: &Connection,
    team: &str,
    member: &str,
    kind: ResourceKind,
    name: &str,
) -> Tx<Option<Quota>> {
    Ok(c.query_row(
        "SELECT limit_units,window_seconds FROM identity_team_quotas
         WHERE team_id=?1 AND kind=?3 AND name=?4 AND member_id IN (?2,'*')
         ORDER BY member_id='*' LIMIT 1",
        params![team, member, kind.as_str(), name],
        |r| {
            Ok(Quota {
                limit: r.get(0)?,
                window: r.get(1)?,
            })
        },
    )
    .optional()?)
}

fn used_in(
    c: &Connection,
    team: &str,
    member: &str,
    kind: &str,
    name: &str,
    window_start: i64,
) -> Tx<i64> {
    Ok(c.query_row(
        "SELECT used FROM identity_team_usage WHERE team_id=?1 AND member_id=?2 AND kind=?3 AND name=?4 AND window_start=?5",
        params![team, member, kind, name, window_start],
        |r| r.get(0),
    )
    .optional()?
    .unwrap_or(0))
}

fn window_start(now: i64, window: i64) -> i64 {
    now - now.rem_euclid(window)
}

fn build_view(
    c: &Connection,
    team: &str,
    s: &AuthenticatedSession,
    role: TeamRole,
    now: i64,
) -> Tx<TeamView> {
    let name: String = c.query_row("SELECT name FROM identity_teams WHERE id=?1", [team], |r| {
        r.get(0)
    })?;
    let members = {
        let mut st = c.prepare(
            "SELECT m.principal_id,p.username,m.role FROM identity_team_members m
             JOIN identity_principals p ON p.id=m.principal_id WHERE m.team_id=?1 ORDER BY m.joined_at,m.principal_id",
        )?;
        st.query_map([team], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|(principal_id, username, role)| {
            Some(MemberView {
                principal_id,
                username,
                role: TeamRole::parse(&role)?,
            })
        })
        .collect()
    };
    let shares: Vec<SharedResourceView> = {
        let mut st = c.prepare(
            "SELECT kind,name FROM identity_team_shares WHERE team_id=?1 ORDER BY kind,name",
        )?;
        st.query_map([team], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|(kind, name)| {
            Some(SharedResourceView {
                kind: ResourceKind::parse(&kind)?,
                name,
            })
        })
        .collect()
    };
    let mut my_quotas = Vec::new();
    for share in &shares {
        if let Some(q) = effective_quota(c, team, &s.principal_id, share.kind, &share.name)? {
            let start = window_start(now, q.window);
            let used = used_in(
                c,
                team,
                &s.principal_id,
                share.kind.as_str(),
                &share.name,
                start,
            )?;
            my_quotas.push(MemberQuotaView {
                kind: share.kind,
                name: share.name.clone(),
                limit_units: q.limit as u64,
                window_seconds: q.window,
                used_units: used as u64,
                window_ends_at: start + q.window,
            });
        }
    }
    Ok(TeamView {
        team_id: team.to_owned(),
        name,
        my_role: role,
        members,
        shares,
        my_quotas,
    })
}

impl Identity {
    /// Creates a team hosted by the caller.
    pub async fn create_team(&self, session: &AuthenticatedSession, name: &str) -> Result<String> {
        if !valid_label(name) {
            return Err(IdentityError::Invalid("team name"));
        }
        let (s, name, now, id) = (
            session.clone(),
            name.to_owned(),
            self.now(),
            crypto::new_id("tem")?,
        );
        self.tx(move |c| {
            live(c, &s, now)?;
            c.execute(
                "INSERT INTO identity_teams(id,host_principal_id,name,created_at) VALUES(?1,?2,?3,?4)",
                params![id, s.principal_id, name, now],
            )?;
            c.execute(
                "INSERT INTO identity_team_members(team_id,principal_id,role,joined_at) VALUES(?1,?2,'host',?3)",
                params![id, s.principal_id, now],
            )?;
            Ok(id)
        })
        .await
    }

    /// Teams the caller belongs to, with the caller's role.
    pub async fn list_teams(
        &self,
        session: &AuthenticatedSession,
    ) -> Result<Vec<(String, String, TeamRole)>> {
        let (s, now) = (session.clone(), self.now());
        self.read(move |c| {
            live(c, &s, now)?;
            let mut st = c.prepare(
                "SELECT t.id,t.name,m.role FROM identity_team_members m JOIN identity_teams t ON t.id=m.team_id
                 WHERE m.principal_id=?1 ORDER BY t.created_at,t.id",
            )?;
            let rows = st
                .query_map([&s.principal_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows
                .into_iter()
                .filter_map(|(id, name, role)| Some((id, name, TeamRole::parse(&role)?)))
                .collect())
        })
        .await
    }

    /// Single-use invite. Admins may invite members; only the host invites admins.
    pub async fn create_team_invite(
        &self,
        session: &AuthenticatedSession,
        team_id: &str,
        role: TeamRole,
    ) -> Result<TeamInvite> {
        if role == TeamRole::Host {
            return Err(IdentityError::Invalid("a team has exactly one host"));
        }
        let (s, team, now) = (session.clone(), team_id.to_owned(), self.now());
        let expires = now + self.config.invite_ttl_seconds;
        let code = crypto::new_code()?;
        let hash = crypto::code_hash("team_invite", &code);
        let id = crypto::new_id("inv")?;
        self.tx(move |c| {
            live(c, &s, now)?;
            let mine = member_role(c, &team, &s)?;
            require(mine, if role == TeamRole::Admin { &[TeamRole::Host] } else { &[TeamRole::Host, TeamRole::Admin] })?;
            c.execute(
                "DELETE FROM identity_codes WHERE purpose='team_invite' AND team_id=?1 AND (expires_at<=?2 OR consumed_at IS NOT NULL)",
                params![team, now],
            )?;
            c.execute(
                "INSERT INTO identity_codes(id,purpose,code_hash,issuer_id,team_id,role,created_at,expires_at)
                 VALUES(?1,'team_invite',?2,?3,?4,?5,?6,?7)",
                params![id, &hash[..], s.principal_id, team, role.as_str(), now, expires],
            )?;
            Ok(())
        })
        .await?;
        let display = Zeroizing::new(format!("{}-{}", &code[..5], &code[5..]));
        Ok(TeamInvite {
            code: TeamInviteCode(display),
            expires_at: expires,
        })
    }

    /// Joins a team with an invite code (single use, rate-limited like pairing).
    pub async fn join_team(
        &self,
        session: &AuthenticatedSession,
        source_key: &str,
        code: &str,
    ) -> Result<String> {
        let s = session.clone();
        self.redeem_code("team_invite", source_key, code, move |c, code, now| {
            live(c, &s, now)?;
            let team = code.team_id.clone().ok_or(IdentityError::Unauthenticated)?;
            let role = code.role.clone().ok_or(IdentityError::Unauthenticated)?;
            // The issuer must still be entitled to invite with that role.
            let issuer = role_in(c, &team, &code.issuer_id)?;
            let allowed = matches!(
            (issuer, role.as_str()),
            (Some(TeamRole::Host), _) | (Some(TeamRole::Admin), "member")
        );
            if !allowed {
                return Err(IdentityError::Unauthenticated.into());
            }
            if role_in(c, &team, &s.principal_id)?.is_some() {
                return Err(IdentityError::Conflict.into());
            }
            c.execute(
                "INSERT INTO identity_team_members(team_id,principal_id,role,joined_at) VALUES(?1,?2,?3,?4)",
                params![team, s.principal_id, role, now],
            )?;
            Ok(team)
        })
        .await
    }

    /// Host removes anyone except itself; admins remove plain members only.
    pub async fn remove_member(
        &self,
        session: &AuthenticatedSession,
        team_id: &str,
        principal_id: &str,
    ) -> Result<()> {
        let (s, team, target, now) = (
            session.clone(),
            team_id.to_owned(),
            principal_id.to_owned(),
            self.now(),
        );
        self.tx(move |c| {
            live(c, &s, now)?;
            let mine = member_role(c, &team, &s)?;
            let theirs = role_in(c, &team, &target)?.ok_or(IdentityError::NotFound)?;
            let allowed = matches!(
                (mine, theirs),
                (TeamRole::Host, TeamRole::Admin | TeamRole::Member)
                    | (TeamRole::Admin, TeamRole::Member)
            );
            if !allowed {
                return Err(IdentityError::Forbidden.into());
            }
            drop_member(c, &team, &target)
        })
        .await
    }

    pub async fn leave_team(&self, session: &AuthenticatedSession, team_id: &str) -> Result<()> {
        let (s, team, now) = (session.clone(), team_id.to_owned(), self.now());
        self.tx(move |c| {
            live(c, &s, now)?;
            if member_role(c, &team, &s)? == TeamRole::Host {
                return Err(IdentityError::Invalid("the host cannot leave its team").into());
            }
            drop_member(c, &team, &s.principal_id)
        })
        .await
    }

    /// Host shares a provider (with a host vault reference) or a tool.
    pub async fn set_share(
        &self,
        session: &AuthenticatedSession,
        team_id: &str,
        kind: ResourceKind,
        name: &str,
        credential: Option<SecretRef>,
    ) -> Result<()> {
        if !valid_name(name) {
            return Err(IdentityError::Invalid("resource name"));
        }
        if kind == ResourceKind::Provider && credential.is_none() {
            return Err(IdentityError::Invalid(
                "a shared provider needs a host credential",
            ));
        }
        let (s, team, name, now) = (
            session.clone(),
            team_id.to_owned(),
            name.to_owned(),
            self.now(),
        );
        self.tx(move |c| {
            live(c, &s, now)?;
            require(member_role(c, &team, &s)?, &[TeamRole::Host])?;
            c.execute(
                "INSERT INTO identity_team_shares(team_id,kind,name,credential_ref,created_at) VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(team_id,kind,name) DO UPDATE SET credential_ref=excluded.credential_ref",
                params![team, kind.as_str(), name, credential.as_ref().map(SecretRef::as_str), now],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn remove_share(
        &self,
        session: &AuthenticatedSession,
        team_id: &str,
        kind: ResourceKind,
        name: &str,
    ) -> Result<()> {
        let (s, team, name, now) = (
            session.clone(),
            team_id.to_owned(),
            name.to_owned(),
            self.now(),
        );
        self.tx(move |c| {
            live(c, &s, now)?;
            require(member_role(c, &team, &s)?, &[TeamRole::Host])?;
            if c.execute(
                "DELETE FROM identity_team_shares WHERE team_id=?1 AND kind=?2 AND name=?3",
                params![team, kind.as_str(), name],
            )? != 1
            {
                return Err(IdentityError::NotFound.into());
            }
            c.execute(
                "DELETE FROM identity_team_quotas WHERE team_id=?1 AND kind=?2 AND name=?3",
                params![team, kind.as_str(), name],
            )?;
            Ok(())
        })
        .await
    }

    /// Host or admin sets a quota on a shared resource.
    pub async fn set_quota(
        &self,
        session: &AuthenticatedSession,
        team_id: &str,
        spec: QuotaSpec,
    ) -> Result<()> {
        let limit = units(spec.limit_units).map_err(|_| IdentityError::Invalid("quota limit"))?;
        if spec.window_seconds <= 0 || spec.window_seconds > 366 * 86_400 {
            return Err(IdentityError::Invalid("quota window"));
        }
        let (s, team, now) = (session.clone(), team_id.to_owned(), self.now());
        self.tx(move |c| {
            live(c, &s, now)?;
            require(member_role(c, &team, &s)?, &[TeamRole::Host, TeamRole::Admin])?;
            let shared: bool = c
                .query_row(
                    "SELECT 1 FROM identity_team_shares WHERE team_id=?1 AND kind=?2 AND name=?3",
                    params![team, spec.kind.as_str(), spec.name],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if !shared {
                return Err(IdentityError::NotShared.into());
            }
            let member = match &spec.member_id {
                Some(m) => {
                    role_in(c, &team, m)?.ok_or(IdentityError::NotFound)?;
                    m.clone()
                }
                None => "*".to_owned(),
            };
            c.execute(
                "INSERT INTO identity_team_quotas(team_id,member_id,kind,name,limit_units,window_seconds) VALUES(?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(team_id,member_id,kind,name) DO UPDATE SET limit_units=excluded.limit_units,window_seconds=excluded.window_seconds",
                params![team, member, spec.kind.as_str(), spec.name, limit, spec.window_seconds],
            )?;
            Ok(())
        })
        .await
    }

    /// Member-safe view of a team the caller belongs to.
    pub async fn team_view(
        &self,
        session: &AuthenticatedSession,
        team_id: &str,
    ) -> Result<TeamView> {
        let (s, team, now) = (session.clone(), team_id.to_owned(), self.now());
        self.read(move |c| {
            live(c, &s, now)?;
            let role = member_role(c, &team, &s)?;
            build_view(c, &team, &s, role, now)
        })
        .await
    }

    /// Host-only view including credential references and every quota.
    pub async fn host_view(
        &self,
        session: &AuthenticatedSession,
        team_id: &str,
    ) -> Result<HostTeamView> {
        let (s, team, now) = (session.clone(), team_id.to_owned(), self.now());
        self.read(move |c| {
            live(c, &s, now)?;
            let role = member_role(c, &team, &s)?;
            require(role, &[TeamRole::Host])?;
            let view = build_view(c, &team, &s, role, now)?;
            let mut st = c.prepare("SELECT kind,name,credential_ref FROM identity_team_shares WHERE team_id=?1 ORDER BY kind,name")?;
            let raw = st
                .query_map([&team], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut shares = Vec::new();
            for (kind, name, credential) in raw {
                let kind = ResourceKind::parse(&kind).ok_or(IdentityError::Invalid("stored share"))?;
                let credential = credential
                    .map(SecretRef::parse)
                    .transpose()
                    .map_err(|_| IdentityError::Invalid("stored share"))?;
                shares.push(HostShareView { kind, name, credential });
            }
            let mut st = c.prepare(
                "SELECT member_id,kind,name,limit_units,window_seconds FROM identity_team_quotas WHERE team_id=?1 ORDER BY kind,name,member_id",
            )?;
            let raw = st
                .query_map([&team], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let quotas = raw
                .into_iter()
                .filter_map(|(member, kind, name, limit, window)| {
                    Some(QuotaSpec {
                        member_id: (member != "*").then_some(member),
                        kind: ResourceKind::parse(&kind)?,
                        name,
                        limit_units: limit as u64,
                        window_seconds: window,
                    })
                })
                .collect();
            Ok(HostTeamView { team: view, shares, quotas })
        })
        .await
    }

    /// Admits a member request against a shared resource, charging `units`
    /// to the member's current quota window. Idempotent per `request_key`:
    /// a replay with identical parameters returns the original admission
    /// without charging again; different parameters are a conflict.
    pub async fn admit(
        &self,
        session: &AuthenticatedSession,
        team_id: &str,
        kind: ResourceKind,
        name: &str,
        requested_units: u64,
        request_key: &str,
    ) -> Result<Admission> {
        if request_key.is_empty()
            || request_key.len() > 128
            || request_key.chars().any(char::is_control)
        {
            return Err(IdentityError::Invalid("request key"));
        }
        let (s, team, name, key, now) = (
            session.clone(),
            team_id.to_owned(),
            name.to_owned(),
            request_key.to_owned(),
            self.now(),
        );
        let new_id = crypto::new_id("adm")?;
        self.tx(move |c| {
            live(c, &s, now)?;
            member_role(c, &team, &s)?;
            let amount = units(requested_units)?;
            let fingerprint = crypto::sha256(&[kind.as_str().as_bytes(), name.as_bytes(), &amount.to_be_bytes()]);
            let credential: Option<Option<String>> = c
                .query_row(
                    "SELECT credential_ref FROM identity_team_shares WHERE team_id=?1 AND kind=?2 AND name=?3",
                    params![team, kind.as_str(), name],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(credential) = credential else {
                return Err(IdentityError::NotShared.into());
            };
            let credential = credential
                .map(SecretRef::parse)
                .transpose()
                .map_err(|_| IdentityError::Invalid("stored share"))?;
            let quota = effective_quota(c, &team, &s.principal_id, kind, &name)?.ok_or(IdentityError::QuotaNotConfigured)?;
            let existing: Option<(String, Vec<u8>, i64, i64, i64)> = c
                .query_row(
                    "SELECT id,fingerprint,window_start,window_end,reserved FROM identity_team_admissions
                     WHERE team_id=?1 AND member_id=?2 AND request_key=?3",
                    params![team, s.principal_id, key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;
            let (admission_id, start, end, replayed) = match existing {
                Some((id, stored, start, end, _)) => {
                    if !crypto::ct_eq(&stored, &fingerprint) {
                        return Err(IdentityError::Conflict.into());
                    }
                    (id, start, end, true)
                }
                None => {
                    let start = window_start(now, quota.window);
                    let end = start + quota.window;
                    let used = used_in(c, &team, &s.principal_id, kind.as_str(), &name, start)?;
                    if used.saturating_add(amount) > quota.limit {
                        return Err(IdentityError::QuotaExceeded {
                            remaining: (quota.limit - used).max(0) as u64,
                            window_ends_at: end,
                        }
                        .into());
                    }
                    c.execute(
                        "INSERT INTO identity_team_usage(team_id,member_id,kind,name,window_start,used) VALUES(?1,?2,?3,?4,?5,?6)
                         ON CONFLICT(team_id,member_id,kind,name,window_start) DO UPDATE SET used=used+excluded.used",
                        params![team, s.principal_id, kind.as_str(), name, start, amount],
                    )?;
                    c.execute(
                        "INSERT INTO identity_team_admissions(team_id,member_id,request_key,id,fingerprint,kind,name,window_start,window_end,reserved,created_at)
                         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                        params![team, s.principal_id, key, new_id, &fingerprint[..], kind.as_str(), name, start, end, amount, now],
                    )?;
                    (new_id, start, end, false)
                }
            };
            let used = used_in(c, &team, &s.principal_id, kind.as_str(), &name, start)?;
            Ok(Admission {
                receipt: AdmissionReceipt {
                    admission_id: admission_id.clone(),
                    team_id: team.clone(),
                    kind,
                    name: name.clone(),
                    reserved_units: amount as u64,
                    remaining_units: (quota.limit - used).max(0) as u64,
                    window_ends_at: end,
                    replayed,
                },
                ticket: HostProxyTicket {
                    admission_id,
                    team_id: team,
                    member_id: s.principal_id.clone(),
                    kind,
                    name,
                    credential,
                },
            })
        })
        .await
    }

    /// Host-side settlement with the actual consumption once the proxied call
    /// finishes; refunds unused reservation or records overage. Idempotent for
    /// the same value.
    pub async fn settle(&self, ticket: &HostProxyTicket, actual_units: u64) -> Result<()> {
        let actual = i64::try_from(actual_units)
            .ok()
            .filter(|v| *v <= i64::MAX / 4)
            .ok_or(IdentityError::Invalid("units"))?;
        let (id, team, member) = (
            ticket.admission_id.clone(),
            ticket.team_id.clone(),
            ticket.member_id.clone(),
        );
        self.tx(move |c| {
            let row: Option<(String, String, i64, i64, Option<i64>)> = c
                .query_row(
                    "SELECT kind,name,window_start,reserved,settled FROM identity_team_admissions
                     WHERE id=?1 AND team_id=?2 AND member_id=?3",
                    params![id, team, member],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;
            let Some((kind, name, start, reserved, settled)) = row else {
                return Err(IdentityError::NotFound.into());
            };
            match settled {
                Some(previous) if previous == actual => return Ok(()),
                Some(_) => return Err(IdentityError::Conflict.into()),
                None => {}
            }
            c.execute(
                "UPDATE identity_team_usage SET used=max(0,used-?6+?7)
                 WHERE team_id=?1 AND member_id=?2 AND kind=?3 AND name=?4 AND window_start=?5",
                params![team, member, kind, name, start, reserved, actual],
            )?;
            c.execute(
                "UPDATE identity_team_admissions SET settled=?1 WHERE id=?2",
                params![actual, id],
            )?;
            Ok(())
        })
        .await
    }

    /// Host grants a member explicit access to one shared project.
    pub async fn grant_project(
        &self,
        session: &AuthenticatedSession,
        team_id: &str,
        member_id: &str,
        project: &str,
        permission: ProjectPermission,
    ) -> Result<()> {
        if !valid_name(project) {
            return Err(IdentityError::Invalid("project"));
        }
        let (s, team, member, project, now) = (
            session.clone(),
            team_id.to_owned(),
            member_id.to_owned(),
            project.to_owned(),
            self.now(),
        );
        self.tx(move |c| {
            live(c, &s, now)?;
            require(member_role(c, &team, &s)?, &[TeamRole::Host])?;
            role_in(c, &team, &member)?.ok_or(IdentityError::NotFound)?;
            c.execute(
                "INSERT INTO identity_team_projects(team_id,member_id,project,permission,granted_at) VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(team_id,member_id,project) DO UPDATE SET permission=excluded.permission,granted_at=excluded.granted_at",
                params![team, member, project, permission.as_str(), now],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn revoke_project(
        &self,
        session: &AuthenticatedSession,
        team_id: &str,
        member_id: &str,
        project: &str,
    ) -> Result<()> {
        let (s, team, member, project, now) = (
            session.clone(),
            team_id.to_owned(),
            member_id.to_owned(),
            project.to_owned(),
            self.now(),
        );
        self.tx(move |c| {
            live(c, &s, now)?;
            require(member_role(c, &team, &s)?, &[TeamRole::Host])?;
            if c.execute(
                "DELETE FROM identity_team_projects WHERE team_id=?1 AND member_id=?2 AND project=?3",
                params![team, member, project],
            )? != 1
            {
                return Err(IdentityError::NotFound.into());
            }
            Ok(())
        })
        .await
    }

    /// Whether the caller may use `project` of the team with `needed`. The host
    /// owns its projects; members need an explicit grant.
    pub async fn check_project_access(
        &self,
        session: &AuthenticatedSession,
        team_id: &str,
        project: &str,
        needed: ProjectPermission,
    ) -> Result<bool> {
        let (s, team, project, now) = (
            session.clone(),
            team_id.to_owned(),
            project.to_owned(),
            self.now(),
        );
        self.read(move |c| {
            live(c, &s, now)?;
            if member_role(c, &team, &s)? == TeamRole::Host {
                return Ok(true);
            }
            let granted: Option<String> = c
                .query_row(
                    "SELECT permission FROM identity_team_projects WHERE team_id=?1 AND member_id=?2 AND project=?3",
                    params![team, s.principal_id, project],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(match granted.as_deref() {
                Some("write") => true,
                Some("read") => needed == ProjectPermission::Read,
                _ => false,
            })
        })
        .await
    }
}

fn drop_member(c: &Connection, team: &str, member: &str) -> Tx<()> {
    c.execute(
        "DELETE FROM identity_team_members WHERE team_id=?1 AND principal_id=?2",
        [team, member],
    )?;
    c.execute(
        "DELETE FROM identity_team_quotas WHERE team_id=?1 AND member_id=?2",
        [team, member],
    )?;
    c.execute(
        "DELETE FROM identity_team_projects WHERE team_id=?1 AND member_id=?2",
        [team, member],
    )?;
    Ok(())
}
