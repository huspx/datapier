ALTER TABLE profiles ADD COLUMN revision INTEGER NOT NULL DEFAULT 1 CHECK(revision > 0);
ALTER TABLE app_settings ADD COLUMN revision INTEGER NOT NULL DEFAULT 1 CHECK(revision > 0);
CREATE TABLE mcp_access_rules (
  profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
  scope_kind TEXT NOT NULL,
  scope_key TEXT NOT NULL,
  level TEXT NOT NULL CHECK(level IN ('disabled', 'read_only', 'read_write')),
  PRIMARY KEY(profile_id, scope_kind, scope_key)
);
CREATE TABLE approvals (
  id TEXT PRIMARY KEY NOT NULL,
  execution_id TEXT NOT NULL UNIQUE,
  owner TEXT NOT NULL,
  session_id TEXT NOT NULL,
  profile_id TEXT NOT NULL,
  request TEXT NOT NULL CHECK(json_valid(request)),
  digest TEXT NOT NULL,
  profile_revision INTEGER NOT NULL,
  policy_revision INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  state TEXT NOT NULL,
  decided_at INTEGER,
  decision_source TEXT
);
CREATE TABLE executions_v3 (
  id TEXT NOT NULL PRIMARY KEY,
  owner TEXT NOT NULL,
  workspace_id TEXT NOT NULL,
  method TEXT NOT NULL,
  params TEXT NOT NULL CHECK(json_valid(params)),
  state TEXT NOT NULL CHECK(
    state IN (
      'queued',
      'approval_required',
      'rejected',
      'expired',
      'invalidated',
      'running',
      'cancelling',
      'succeeded',
      'failed',
      'cancelled',
      'outcome_unknown',
      'wait_stopped'
    )
  ),
  sent INTEGER NOT NULL DEFAULT 0 CHECK(sent IN (0, 1)),
  may_write INTEGER NOT NULL DEFAULT 0 CHECK(may_write IN (0, 1)),
  summary TEXT NOT NULL DEFAULT 'null' CHECK(json_valid(summary)),
  error TEXT,
  created_at REAL NOT NULL DEFAULT (unixepoch('subsec')),
  updated_at REAL NOT NULL DEFAULT (unixepoch('subsec'))
);
INSERT INTO executions_v3
SELECT *
FROM executions;
DROP TABLE executions;
ALTER TABLE executions_v3 RENAME TO executions;
CREATE INDEX executions_workspace ON executions(workspace_id, created_at);
