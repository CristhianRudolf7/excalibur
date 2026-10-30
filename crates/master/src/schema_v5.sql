CREATE TABLE master_context_updates(
 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
 principal TEXT NOT NULL,
 conversation TEXT NOT NULL REFERENCES conversations(id),
 scope TEXT NOT NULL,
 source TEXT NOT NULL REFERENCES messages(id),
 update_id TEXT NOT NULL,
 body TEXT NOT NULL,
 UNIQUE(principal,conversation,scope,source,update_id)
);
