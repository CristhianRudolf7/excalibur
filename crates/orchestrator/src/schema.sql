CREATE TABLE IF NOT EXISTS orchestration_schema(version INTEGER PRIMARY KEY,checksum TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS orchestration_agents(id TEXT PRIMARY KEY,parent_id TEXT REFERENCES orchestration_agents(id),run_id TEXT UNIQUE,depth INTEGER NOT NULL,body TEXT NOT NULL,created INTEGER NOT NULL,paused_by TEXT,resource_paused INTEGER NOT NULL DEFAULT 0);
CREATE UNIQUE INDEX IF NOT EXISTS orchestration_one_master ON orchestration_agents(depth) WHERE depth=0;
CREATE TABLE IF NOT EXISTS orchestration_spawn(operation TEXT PRIMARY KEY,request_hash TEXT NOT NULL,agent_id TEXT NOT NULL,body TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS orchestration_projects(id TEXT PRIMARY KEY,body TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS orchestration_controls(operation TEXT PRIMARY KEY,target TEXT NOT NULL,action TEXT NOT NULL,body TEXT NOT NULL,state TEXT NOT NULL,report TEXT);
CREATE TABLE IF NOT EXISTS orchestration_messages(id TEXT PRIMARY KEY,client_id TEXT NOT NULL,sender TEXT NOT NULL,recipient TEXT NOT NULL,kind TEXT NOT NULL,payload TEXT NOT NULL,priority TEXT NOT NULL,created INTEGER NOT NULL,step INTEGER NOT NULL,state TEXT NOT NULL,UNIQUE(sender,client_id));
CREATE INDEX IF NOT EXISTS orchestration_mailbox ON orchestration_messages(recipient,state,created,id);
CREATE TABLE IF NOT EXISTS orchestration_notifications(id TEXT PRIMARY KEY,agent_id TEXT NOT NULL,conversation TEXT NOT NULL,class TEXT NOT NULL,content TEXT NOT NULL,created INTEGER NOT NULL,acknowledged INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS orchestration_deliveries(notification_id TEXT NOT NULL REFERENCES orchestration_notifications(id),channel TEXT NOT NULL,state TEXT NOT NULL,PRIMARY KEY(notification_id,channel));
CREATE TABLE IF NOT EXISTS orchestration_activity(conversation TEXT PRIMARY KEY,channel TEXT NOT NULL,connected INTEGER NOT NULL,last_active INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS orchestration_notify_policy(conversation TEXT PRIMARY KEY,body TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS orchestration_resource_state(id INTEGER PRIMARY KEY CHECK(id=1),high INTEGER NOT NULL,below_since INTEGER);
INSERT OR IGNORE INTO orchestration_resource_state VALUES(1,0,NULL);

CREATE TABLE IF NOT EXISTS orchestration_waits(agent_id TEXT PRIMARY KEY REFERENCES orchestration_agents(id),requested INTEGER NOT NULL);

CREATE TABLE IF NOT EXISTS orchestration_prepared(operation TEXT PRIMARY KEY REFERENCES orchestration_spawn(operation),body TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS orchestration_message_originals(id TEXT PRIMARY KEY REFERENCES orchestration_messages(id),priority TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS orchestration_profiles(id TEXT PRIMARY KEY,body TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS orchestration_agent_controls(message_id TEXT PRIMARY KEY REFERENCES orchestration_messages(id),agent_id TEXT NOT NULL REFERENCES orchestration_agents(id),run_id TEXT NOT NULL REFERENCES runs(id),tool_key TEXT NOT NULL,kind TEXT NOT NULL CHECK(kind IN('user','blocked')),reason TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS orchestration_agent_control_owner ON orchestration_agent_controls(agent_id,kind);
