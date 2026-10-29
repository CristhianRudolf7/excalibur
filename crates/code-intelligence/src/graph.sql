CREATE TABLE ci_project(project_id TEXT PRIMARY KEY NOT NULL);
CREATE TABLE ci_file_meta(file_id TEXT PRIMARY KEY NOT NULL REFERENCES files(id) ON DELETE CASCADE,grammar TEXT NOT NULL,syntax_errors TEXT NOT NULL CHECK(json_valid(syntax_errors)));
CREATE TABLE ci_node_meta(node_id TEXT PRIMARY KEY NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,qualified_name TEXT NOT NULL,start_line INTEGER NOT NULL,start_character INTEGER NOT NULL,end_line INTEGER NOT NULL,end_character INTEGER NOT NULL,attributes TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(attributes)));
CREATE TABLE ci_edge_meta(edge_id TEXT PRIMARY KEY NOT NULL REFERENCES edges(id) ON DELETE CASCADE,resolution TEXT NOT NULL);
CREATE TABLE ci_references(id TEXT PRIMARY KEY NOT NULL,source_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,kind TEXT NOT NULL,target_name TEXT NOT NULL,target_path TEXT,start_byte INTEGER NOT NULL);
CREATE INDEX ci_reference_target ON ci_references(target_name,target_path);
