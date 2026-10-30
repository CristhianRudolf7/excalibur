CREATE TABLE master_cancel_confirmations(
 question_id TEXT PRIMARY KEY REFERENCES master_questions(id),
 conversation_id TEXT NOT NULL REFERENCES conversations(id),
 principal_id TEXT NOT NULL REFERENCES principals(id),
 scope_id TEXT NOT NULL,
 target TEXT NOT NULL,
 source_message_id TEXT NOT NULL UNIQUE REFERENCES messages(id),
 answer_message_id TEXT UNIQUE REFERENCES messages(id),
 state TEXT NOT NULL DEFAULT 'pending' CHECK(state IN ('pending','accepted','rejected','done')),
 result TEXT
);
CREATE UNIQUE INDEX master_cancel_pending_target ON master_cancel_confirmations(conversation_id,principal_id,scope_id,target) WHERE state='pending';
