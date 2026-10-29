CREATE TABLE IF NOT EXISTS workspace_assignment_parents(root TEXT NOT NULL,task TEXT NOT NULL,parent TEXT NOT NULL,PRIMARY KEY(root,task));
CREATE TABLE IF NOT EXISTS workspace_assignments(root TEXT NOT NULL,task TEXT NOT NULL,owner TEXT NOT NULL,path TEXT NOT NULL,PRIMARY KEY(root,task,owner,path),FOREIGN KEY(root,task) REFERENCES workspace_assignment_parents(root,task));
