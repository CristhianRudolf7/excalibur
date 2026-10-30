DROP INDEX master_cancel_pending_target;
ALTER TABLE master_cancel_confirmations RENAME TO master_cancel_confirmations_v3;
CREATE TABLE master_cancel_confirmations(
 question_id TEXT PRIMARY KEY REFERENCES master_questions(id),
 conversation_id TEXT NOT NULL REFERENCES conversations(id),
 principal_id TEXT NOT NULL REFERENCES principals(id),
 scope_id TEXT NOT NULL,
 target TEXT NOT NULL,
 source_message_id TEXT NOT NULL REFERENCES messages(id),
 request_key TEXT NOT NULL UNIQUE,
 answer_message_id TEXT UNIQUE REFERENCES messages(id),
 state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','accepted','rejected','done')),
 result TEXT
);
INSERT INTO master_cancel_confirmations
SELECT question_id,conversation_id,principal_id,scope_id,target,source_message_id,
 COALESCE((SELECT operation_key FROM master_action_receipts r WHERE r.conversation_id=x.conversation_id AND r.tool_name='subagent' AND json_extract(r.arguments,'$.action')='cancel' AND json_extract(r.arguments,'$.target')=x.target AND substr(r.operation_key,1,length('master:'||x.conversation_id||':'||x.source_message_id||':'))='master:'||x.conversation_id||':'||x.source_message_id||':' ORDER BY operation_key LIMIT 1),'message:'||source_message_id),
 answer_message_id,state,result FROM master_cancel_confirmations_v3 x;
DROP TABLE master_cancel_confirmations_v3;
CREATE UNIQUE INDEX master_cancel_pending_target ON master_cancel_confirmations(conversation_id,principal_id,scope_id,target) WHERE state='pending';
