-- Host bindings, keyed tool receipts, entity merges and curation history.
CREATE TABLE memory_run_bindings(run_id TEXT PRIMARY KEY NOT NULL,principal_id TEXT NOT NULL,scopes TEXT NOT NULL CHECK(json_valid(scopes)),primary_scope TEXT NOT NULL,conversation_id TEXT,task_id TEXT,parent_run_id TEXT,role TEXT NOT NULL,bound_at INTEGER NOT NULL);
CREATE INDEX memory_run_bindings_parent ON memory_run_bindings(parent_run_id);
CREATE INDEX memory_run_bindings_conversation ON memory_run_bindings(conversation_id);
CREATE TABLE memory_tool_receipts(run_id TEXT NOT NULL,key TEXT NOT NULL,tool TEXT NOT NULL,request_hash TEXT NOT NULL,result TEXT NOT NULL CHECK(json_valid(result)),at INTEGER NOT NULL,PRIMARY KEY(run_id,key));
ALTER TABLE entities ADD COLUMN merged_into TEXT;
CREATE TABLE memory_curations(id INTEGER PRIMARY KEY,principal_id TEXT NOT NULL,scope_id TEXT NOT NULL,action TEXT NOT NULL,subject TEXT NOT NULL,detail TEXT NOT NULL CHECK(json_valid(detail)),actor TEXT,at INTEGER NOT NULL);
CREATE INDEX memory_curations_subject ON memory_curations(subject);
CREATE TABLE memory_maintenance(principal_id TEXT NOT NULL,job TEXT NOT NULL,last_at INTEGER NOT NULL,detail TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(detail)),PRIMARY KEY(principal_id,job));
