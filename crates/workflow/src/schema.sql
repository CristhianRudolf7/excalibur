CREATE TABLE IF NOT EXISTS workflow_definitions(
    scope TEXT NOT NULL,
    workflow_id TEXT NOT NULL,
    version INTEGER NOT NULL CHECK(version > 0),
    definition TEXT NOT NULL CHECK(json_valid(definition)),
    hash TEXT NOT NULL,
    published_at INTEGER NOT NULL,
    PRIMARY KEY(scope, workflow_id, version)
);
CREATE TRIGGER IF NOT EXISTS workflow_definitions_immutable
BEFORE UPDATE ON workflow_definitions
BEGIN
    SELECT RAISE(ABORT, 'workflow definitions are immutable');
END;
CREATE TRIGGER IF NOT EXISTS workflow_definitions_kept
BEFORE DELETE ON workflow_definitions
BEGIN
    SELECT RAISE(ABORT, 'workflow definitions are immutable');
END;
CREATE TABLE IF NOT EXISTS workflow_instances(
    scope TEXT NOT NULL,
    id TEXT NOT NULL,
    workflow_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    definition_hash TEXT NOT NULL,
    mode TEXT NOT NULL CHECK(mode IN ('live', 'dry_run')),
    input TEXT NOT NULL CHECK(json_valid(input)),
    status TEXT NOT NULL CHECK(status IN ('running', 'waiting', 'waiting_approval', 'paused',
        'blocked', 'failed', 'compensating', 'completed', 'aborted')),
    cursor TEXT,
    wake_at INTEGER,
    failure_class TEXT,
    reason TEXT,
    revision INTEGER NOT NULL DEFAULT 0,
    next_seq INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY(scope, id),
    FOREIGN KEY(scope, workflow_id, version)
        REFERENCES workflow_definitions(scope, workflow_id, version)
);
-- Per-node durable progress. Acyclic graphs visit each node at most once.
CREATE TABLE IF NOT EXISTS workflow_steps(
    scope TEXT NOT NULL,
    instance_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    phase TEXT NOT NULL CHECK(phase IN ('intent', 'retry', 'failed', 'waiting', 'done',
        'compensating', 'compensated')),
    idempotency_key TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    logical_failures INTEGER NOT NULL DEFAULT 0,
    output TEXT CHECK(output IS NULL OR json_valid(output)),
    wake_at INTEGER,
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    PRIMARY KEY(scope, instance_id, node_id),
    FOREIGN KEY(scope, instance_id) REFERENCES workflow_instances(scope, id)
);
CREATE TABLE IF NOT EXISTS workflow_approvals(
    scope TEXT NOT NULL,
    instance_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    prompt TEXT NOT NULL,
    requested_at INTEGER NOT NULL,
    approved INTEGER CHECK(approved IS NULL OR approved IN (0, 1)),
    decided_by TEXT,
    decided_at INTEGER,
    PRIMARY KEY(scope, instance_id, node_id),
    FOREIGN KEY(scope, instance_id) REFERENCES workflow_instances(scope, id)
);
-- Append-only evidence: attempts, retries, keys, simulations, approvals, compensations.
CREATE TABLE IF NOT EXISTS workflow_log(
    scope TEXT NOT NULL,
    instance_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    at INTEGER NOT NULL,
    node_id TEXT,
    kind TEXT NOT NULL,
    idempotency_key TEXT,
    attempt INTEGER,
    detail TEXT NOT NULL CHECK(json_valid(detail)),
    PRIMARY KEY(scope, instance_id, seq),
    FOREIGN KEY(scope, instance_id) REFERENCES workflow_instances(scope, id)
);
CREATE TRIGGER IF NOT EXISTS workflow_log_append_only
BEFORE UPDATE ON workflow_log
BEGIN
    SELECT RAISE(ABORT, 'workflow log is append-only');
END;
