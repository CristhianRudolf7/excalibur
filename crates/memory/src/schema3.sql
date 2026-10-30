-- Preserve the full host authority and fail closed across interrupted keyed effects.
ALTER TABLE memory_run_bindings ADD COLUMN timezone TEXT NOT NULL DEFAULT 'UTC';
ALTER TABLE memory_tool_receipts ADD COLUMN state TEXT NOT NULL DEFAULT 'completed' CHECK(state IN ('pending','completed'));
ALTER TABLE profile_proposals ADD COLUMN auto_confirmable INTEGER NOT NULL DEFAULT 0 CHECK(auto_confirmable IN (0,1));
