-- Paths, code and symbol identifiers use substring-aware trigram search.
ALTER TABLE files ADD COLUMN content TEXT NOT NULL DEFAULT '';
CREATE VIRTUAL TABLE files_fts USING fts5(path,content,content='files',content_rowid='rowid',tokenize='trigram');
CREATE TRIGGER files_fts_insert AFTER INSERT ON files BEGIN INSERT INTO files_fts(rowid,path,content) VALUES(NEW.rowid,NEW.path,NEW.content); END;
CREATE TRIGGER files_fts_delete AFTER DELETE ON files BEGIN INSERT INTO files_fts(files_fts,rowid,path,content) VALUES('delete',OLD.rowid,OLD.path,OLD.content); END;
CREATE TRIGGER files_fts_update AFTER UPDATE OF path,content ON files BEGIN INSERT INTO files_fts(files_fts,rowid,path,content) VALUES('delete',OLD.rowid,OLD.path,OLD.content); INSERT INTO files_fts(rowid,path,content) VALUES(NEW.rowid,NEW.path,NEW.content); END;
INSERT INTO files_fts(files_fts) VALUES('rebuild');
CREATE VIRTUAL TABLE nodes_fts USING fts5(name,content='nodes',content_rowid='rowid',tokenize='trigram');
CREATE TRIGGER nodes_fts_insert AFTER INSERT ON nodes BEGIN INSERT INTO nodes_fts(rowid,name) VALUES(NEW.rowid,NEW.name); END;
CREATE TRIGGER nodes_fts_delete AFTER DELETE ON nodes BEGIN INSERT INTO nodes_fts(nodes_fts,rowid,name) VALUES('delete',OLD.rowid,OLD.name); END;
CREATE TRIGGER nodes_fts_update AFTER UPDATE OF name ON nodes BEGIN INSERT INTO nodes_fts(nodes_fts,rowid,name) VALUES('delete',OLD.rowid,OLD.name); INSERT INTO nodes_fts(rowid,name) VALUES(NEW.rowid,NEW.name); END;
INSERT INTO nodes_fts(nodes_fts) VALUES('rebuild');
