CREATE TABLE archery_servers (
  id TEXT NOT NULL PRIMARY KEY,
  name TEXT NOT NULL,
  base_url TEXT NOT NULL,
  username TEXT NOT NULL,
  http_tls TEXT NOT NULL CHECK(json_valid(http_tls)),
  revision INTEGER NOT NULL CHECK(revision > 0),
  credential_saved INTEGER NOT NULL DEFAULT 0 CHECK(credential_saved IN (0, 1)),
  totp_mode TEXT NOT NULL DEFAULT 'manual' CHECK(totp_mode IN ('manual', 'automatic')),
  totp_credential_saved INTEGER NOT NULL DEFAULT 0 CHECK(totp_credential_saved IN (0, 1)),
  UNIQUE(base_url, username)
);

CREATE TABLE profiles_new (
  id TEXT NOT NULL PRIMARY KEY,
  name TEXT NOT NULL,
  payload TEXT NOT NULL CHECK(json_valid(payload)),
  credential_saved INTEGER NOT NULL DEFAULT 0 CHECK(credential_saved IN (0, 1)),
  archery_server_id TEXT REFERENCES archery_servers(id) ON DELETE RESTRICT,
  archery_instance_id INTEGER,
  CHECK (
    (archery_server_id IS NULL AND archery_instance_id IS NULL)
    OR (archery_server_id IS NOT NULL AND archery_instance_id IS NOT NULL AND credential_saved = 0)
  )
);
INSERT INTO profiles_new (id, name, payload, credential_saved)
SELECT id, name, payload, credential_saved
FROM profiles;
DROP TABLE profiles;
ALTER TABLE profiles_new RENAME TO profiles;
CREATE UNIQUE INDEX profiles_archery_instance ON profiles(archery_server_id, archery_instance_id);

CREATE TABLE workspaces_new (
  id TEXT NOT NULL PRIMARY KEY,
  revision INTEGER NOT NULL CHECK(revision > 0),
  payload TEXT NOT NULL CHECK(json_valid(payload))
);
INSERT INTO workspaces_new
SELECT id, revision, payload
FROM workspaces;
DROP TABLE workspaces;
ALTER TABLE workspaces_new RENAME TO workspaces;

CREATE TABLE executions_new (
  id TEXT NOT NULL PRIMARY KEY,
  owner TEXT NOT NULL,
  workspace_id TEXT NOT NULL,
  method TEXT NOT NULL,
  params TEXT NOT NULL CHECK(json_valid(params)),
  state TEXT NOT NULL CHECK(
    state IN (
      'queued',
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
INSERT INTO executions_new
SELECT id, owner, workspace_id, method, params, state, sent, may_write, summary, error, created_at, updated_at
FROM executions;
-- schema 1 只支持直连，保留原有摘要字段及全部历史参数。
UPDATE executions_new
SET summary = json_set(summary, '$._migration', json_object('source', 'direct', 'config_schema', 1))
WHERE json_type(summary) = 'object';
DROP TABLE executions;
ALTER TABLE executions_new RENAME TO executions;
CREATE INDEX executions_workspace ON executions(workspace_id, created_at);

CREATE TABLE app_settings_new (
  id INTEGER PRIMARY KEY CHECK(id = 1),
  payload TEXT NOT NULL CHECK(json_valid(payload))
);
INSERT INTO app_settings_new
SELECT id, payload
FROM app_settings;
DROP TABLE app_settings;
ALTER TABLE app_settings_new RENAME TO app_settings;
