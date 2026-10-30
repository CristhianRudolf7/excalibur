CREATE TABLE IF NOT EXISTS scheduler_timezone_changes(
    scope TEXT NOT NULL,
    change_key TEXT NOT NULL,
    note TEXT NOT NULL CHECK(json_valid(note)),
    old_definition TEXT NOT NULL CHECK(json_valid(old_definition)),
    new_definition TEXT NOT NULL CHECK(json_valid(new_definition)),
    delivered INTEGER NOT NULL DEFAULT 0 CHECK(delivered IN(0,1)),
    PRIMARY KEY(scope,change_key)
);
