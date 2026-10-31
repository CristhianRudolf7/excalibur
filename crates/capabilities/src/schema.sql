CREATE TABLE IF NOT EXISTS capability_manifests(
    capability TEXT NOT NULL,
    version TEXT NOT NULL,
    definition TEXT NOT NULL CHECK(json_valid(definition)),
    PRIMARY KEY(capability,version)
);
CREATE TABLE IF NOT EXISTS capability_tasks(
    scope TEXT NOT NULL,
    id TEXT NOT NULL,
    operation_key TEXT NOT NULL,
    capability TEXT NOT NULL,
    capability_version TEXT NOT NULL,
    operation TEXT NOT NULL,
    definition TEXT NOT NULL CHECK(json_valid(definition)),
    input TEXT NOT NULL CHECK(json_valid(input)),
    state TEXT NOT NULL CHECK(state IN ('working','input_required','completed','failed','cancelled')),
    remote_id TEXT,
    start_attempts INTEGER NOT NULL DEFAULT 0 CHECK(start_attempts>=0),
    input_request TEXT CHECK(input_request IS NULL OR json_valid(input_request)),
    input_seq INTEGER NOT NULL DEFAULT 0 CHECK(input_seq>=0),
    pending_input TEXT CHECK(pending_input IS NULL OR json_valid(pending_input)),
    cancel_state TEXT NOT NULL DEFAULT 'none' CHECK(cancel_state IN ('none','requested','sent')),
    cancel_unhonored INTEGER NOT NULL DEFAULT 0 CHECK(cancel_unhonored IN (0,1)),
    result TEXT CHECK(result IS NULL OR json_valid(result)),
    failure TEXT CHECK(failure IS NULL OR json_valid(failure)),
    failures INTEGER NOT NULL DEFAULT 0 CHECK(failures>=0),
    in_flight INTEGER NOT NULL DEFAULT 0 CHECK(in_flight IN (0,1)),
    claim_seq INTEGER NOT NULL DEFAULT 0 CHECK(claim_seq>=0),
    next_poll_at INTEGER NOT NULL,
    deadline_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    version INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(scope,id),
    UNIQUE(scope,operation_key),
    FOREIGN KEY(capability,capability_version) REFERENCES capability_manifests(capability,version)
);
CREATE INDEX IF NOT EXISTS capability_tasks_due ON capability_tasks(scope,next_poll_at)
    WHERE state IN ('working','input_required');
CREATE INDEX IF NOT EXISTS capability_tasks_active ON capability_tasks(capability)
    WHERE state IN ('working','input_required');
CREATE TABLE IF NOT EXISTS capability_task_events(
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL,
    task_id TEXT NOT NULL,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    detail TEXT NOT NULL CHECK(json_valid(detail))
);
CREATE INDEX IF NOT EXISTS capability_task_events_task ON capability_task_events(scope,task_id,seq);
