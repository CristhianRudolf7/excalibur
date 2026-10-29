-- Durable admission-to-dispatch boundary: absence of dispatch proves the effect was not called.
CREATE TABLE IF NOT EXISTS security_dispatch(ticket TEXT PRIMARY KEY REFERENCES security_admissions(ticket),dispatched INTEGER NOT NULL DEFAULT 0,result TEXT CHECK(result IS NULL OR json_valid(result)));
