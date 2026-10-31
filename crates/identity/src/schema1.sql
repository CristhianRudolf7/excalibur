CREATE TABLE IF NOT EXISTS identity_principals(
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    role TEXT NOT NULL CHECK(role IN ('owner','user')),
    created_at INTEGER NOT NULL,
    disabled_at INTEGER
);
CREATE TABLE IF NOT EXISTS identity_passwords(
    principal_id TEXT PRIMARY KEY,
    algorithm TEXT NOT NULL CHECK(algorithm='argon2id'),
    version INTEGER NOT NULL CHECK(version=19),
    m_cost INTEGER NOT NULL CHECK(m_cost>=8),
    t_cost INTEGER NOT NULL CHECK(t_cost>=1),
    p_cost INTEGER NOT NULL CHECK(p_cost>=1),
    salt BLOB NOT NULL CHECK(length(salt)=16),
    hash BLOB NOT NULL CHECK(length(hash)=32),
    generation INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS identity_devices(
    id TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL,
    name TEXT NOT NULL,
    public_key BLOB NOT NULL UNIQUE CHECK(length(public_key)=32),
    enrolled_via TEXT NOT NULL CHECK(enrolled_via IN ('local','pairing')),
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
);
CREATE INDEX IF NOT EXISTS identity_devices_principal ON identity_devices(principal_id);
CREATE TABLE IF NOT EXISTS identity_challenges(
    id TEXT PRIMARY KEY,
    device_id TEXT NOT NULL,
    nonce BLOB NOT NULL CHECK(length(nonce)=32),
    expires_at INTEGER NOT NULL,
    consumed_at INTEGER
);
CREATE TABLE IF NOT EXISTS identity_sessions(
    id TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL,
    device_id TEXT NOT NULL,
    secret_hash BLOB NOT NULL CHECK(length(secret_hash)=32),
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    idle_expires_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked_at INTEGER,
    revoke_reason TEXT
);
CREATE INDEX IF NOT EXISTS identity_sessions_principal ON identity_sessions(principal_id);
CREATE INDEX IF NOT EXISTS identity_sessions_device ON identity_sessions(device_id);
CREATE TABLE IF NOT EXISTS identity_csrf(
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    secret_hash BLOB NOT NULL CHECK(length(secret_hash)=32),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS identity_csrf_session ON identity_csrf(session_id);
CREATE TABLE IF NOT EXISTS identity_codes(
    id TEXT PRIMARY KEY,
    purpose TEXT NOT NULL CHECK(purpose IN ('pairing','team_invite')),
    code_hash BLOB NOT NULL UNIQUE CHECK(length(code_hash)=32),
    issuer_id TEXT NOT NULL,
    team_id TEXT,
    role TEXT CHECK(role IS NULL OR role IN ('admin','member')),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    consumed_at INTEGER,
    consumed_by TEXT,
    CHECK((purpose='team_invite')=(team_id IS NOT NULL AND role IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS identity_codes_issuer ON identity_codes(issuer_id,purpose);
CREATE TABLE IF NOT EXISTS identity_rate(
    scope TEXT NOT NULL,
    key TEXT NOT NULL,
    window_start INTEGER NOT NULL,
    failures INTEGER NOT NULL CHECK(failures>=0),
    PRIMARY KEY(scope,key)
);
CREATE TABLE IF NOT EXISTS identity_teams(
    id TEXT PRIMARY KEY,
    host_principal_id TEXT NOT NULL,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS identity_team_members(
    team_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK(role IN ('host','admin','member')),
    joined_at INTEGER NOT NULL,
    PRIMARY KEY(team_id,principal_id)
);
CREATE INDEX IF NOT EXISTS identity_team_members_principal ON identity_team_members(principal_id);
CREATE TABLE IF NOT EXISTS identity_team_shares(
    team_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('provider','tool')),
    name TEXT NOT NULL,
    credential_ref TEXT,
    created_at INTEGER NOT NULL,
    PRIMARY KEY(team_id,kind,name),
    CHECK(kind='tool' OR credential_ref IS NOT NULL)
);
CREATE TABLE IF NOT EXISTS identity_team_quotas(
    team_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('provider','tool')),
    name TEXT NOT NULL,
    limit_units INTEGER NOT NULL CHECK(limit_units>0),
    window_seconds INTEGER NOT NULL CHECK(window_seconds>0),
    PRIMARY KEY(team_id,member_id,kind,name)
);
CREATE TABLE IF NOT EXISTS identity_team_usage(
    team_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    window_start INTEGER NOT NULL,
    used INTEGER NOT NULL CHECK(used>=0),
    PRIMARY KEY(team_id,member_id,kind,name,window_start)
);
CREATE TABLE IF NOT EXISTS identity_team_admissions(
    team_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    request_key TEXT NOT NULL,
    id TEXT NOT NULL UNIQUE,
    fingerprint BLOB NOT NULL CHECK(length(fingerprint)=32),
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    window_start INTEGER NOT NULL,
    window_end INTEGER NOT NULL,
    reserved INTEGER NOT NULL CHECK(reserved>0),
    settled INTEGER CHECK(settled IS NULL OR settled>=0),
    created_at INTEGER NOT NULL,
    PRIMARY KEY(team_id,member_id,request_key)
);
CREATE TABLE IF NOT EXISTS identity_team_projects(
    team_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    project TEXT NOT NULL,
    permission TEXT NOT NULL CHECK(permission IN ('read','write')),
    granted_at INTEGER NOT NULL,
    PRIMARY KEY(team_id,member_id,project)
);
