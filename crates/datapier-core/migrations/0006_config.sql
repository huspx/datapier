CREATE TABLE local_projects (
  id TEXT NOT NULL PRIMARY KEY,
  name TEXT NOT NULL CHECK(length(name) > 0),
  description TEXT NOT NULL,
  default_environment_id TEXT,
  revision INTEGER NOT NULL CHECK(revision > 0)
);
CREATE TABLE local_project_roots (
  path TEXT NOT NULL PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES local_projects(id) ON DELETE CASCADE,
  identity TEXT NOT NULL
);
CREATE TABLE local_project_environments (
  id TEXT NOT NULL PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES local_projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL CHECK(length(name) > 0),
  environment_type TEXT NOT NULL,
  description TEXT NOT NULL,
  UNIQUE(project_id, name)
);
ALTER TABLE project_bindings ADD COLUMN description TEXT NOT NULL DEFAULT '';
ALTER TABLE project_bindings ADD COLUMN profile_identity TEXT;
CREATE TRIGGER local_binding_insert AFTER INSERT ON project_bindings BEGIN
  UPDATE local_projects SET revision = revision + 1 WHERE id = NEW.project_id AND EXISTS (SELECT 1 FROM local_project_environments WHERE id = NEW.binding_group AND project_id = NEW.project_id);
END;
CREATE TRIGGER local_binding_update AFTER UPDATE ON project_bindings BEGIN
  UPDATE local_projects SET revision = revision + 1 WHERE id = NEW.project_id AND EXISTS (SELECT 1 FROM local_project_environments WHERE id = NEW.binding_group AND project_id = NEW.project_id);
END;
CREATE TRIGGER local_binding_delete AFTER DELETE ON project_bindings BEGIN
  UPDATE local_projects SET revision = revision + 1 WHERE id = OLD.project_id AND EXISTS (SELECT 1 FROM local_project_environments WHERE id = OLD.binding_group AND project_id = OLD.project_id);
END;
PRAGMA user_version = 6;
