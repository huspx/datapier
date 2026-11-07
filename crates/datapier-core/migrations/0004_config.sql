CREATE TABLE value_edit_files (
  id TEXT NOT NULL PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  profile_id TEXT NOT NULL,
  relative_dir TEXT NOT NULL UNIQUE,
  purpose TEXT NOT NULL CHECK (purpose IN ('external', 'draft')),
  target_json TEXT NOT NULL CHECK (json_valid(target_json)),
  state TEXT NOT NULL CHECK (state IN ('preparing', 'ready', 'opened', 'returned', 'orphaned', 'cleanup_pending')),
  format TEXT NOT NULL,
  baseline_digest TEXT NOT NULL,
  draft_revision INTEGER NOT NULL CHECK (draft_revision > 0),
  revision INTEGER NOT NULL CHECK (revision > 0),
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
CREATE INDEX value_edit_files_workspace_state ON value_edit_files (workspace_id, state);
PRAGMA user_version = 4;
