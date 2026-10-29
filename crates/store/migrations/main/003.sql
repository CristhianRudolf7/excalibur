-- Natural-language index normalization; names and columns remain readable by older binaries.
DROP TABLE messages_fts;
CREATE VIRTUAL TABLE messages_fts USING fts5(text,content='messages',content_rowid='rowid',tokenize='unicode61 remove_diacritics 2');
INSERT INTO messages_fts(messages_fts) VALUES('rebuild');
DROP TABLE memories_fts;
CREATE VIRTUAL TABLE memories_fts USING fts5(statement,content='memories',content_rowid='rowid',tokenize='unicode61 remove_diacritics 2');
INSERT INTO memories_fts(memories_fts) VALUES('rebuild');
DROP TABLE doc_fragments_fts;
CREATE VIRTUAL TABLE doc_fragments_fts USING fts5(text,content='doc_fragments',content_rowid='rowid',tokenize='unicode61 remove_diacritics 2');
INSERT INTO doc_fragments_fts(doc_fragments_fts) VALUES('rebuild');
DROP TABLE doc_chunks_fts;
CREATE VIRTUAL TABLE doc_chunks_fts USING fts5(text,content='doc_chunks',content_rowid='rowid',tokenize='unicode61 remove_diacritics 2');
INSERT INTO doc_chunks_fts(doc_chunks_fts) VALUES('rebuild');
-- Legacy vector tables remain intact; the transactional Rust migration copies them into scoped vec0 indexes.
CREATE TABLE vector_indexes(space_id TEXT PRIMARY KEY NOT NULL,table_name TEXT UNIQUE NOT NULL,dimensions INTEGER NOT NULL CHECK(dimensions BETWEEN 1 AND 8192),element_type TEXT NOT NULL CHECK(element_type IN ('float32','int8','bit')),created_at INTEGER NOT NULL DEFAULT(unixepoch()));
