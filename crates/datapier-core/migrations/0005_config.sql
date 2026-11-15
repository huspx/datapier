CREATE TABLE transfer_presets (
  id TEXT NOT NULL PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision > 0),
  name TEXT NOT NULL CHECK(length(name) > 0),
  spec_json TEXT NOT NULL CHECK(json_valid(spec_json)),
  updated_at_ms INTEGER NOT NULL
);
CREATE TABLE transfer_runs (
  id TEXT NOT NULL PRIMARY KEY,
  execution_id TEXT NOT NULL UNIQUE REFERENCES executions(id) ON DELETE RESTRICT,
  workspace_id TEXT NOT NULL,
  spec_digest TEXT NOT NULL CHECK(length(spec_digest) = 64 AND spec_digest NOT GLOB '*[^0-9a-f]*'),
  context_json TEXT NOT NULL CHECK(json_valid(context_json)),
  state TEXT NOT NULL CHECK(state IN ('running', 'cancelling', 'succeeded', 'failed', 'cancelled', 'outcome_unknown')),
  progress_json TEXT NOT NULL CHECK(json_valid(progress_json)),
  retry_of TEXT REFERENCES transfer_runs(id) ON DELETE RESTRICT,
  revision INTEGER NOT NULL CHECK(revision > 0),
  created_at_ms INTEGER NOT NULL,
  finished_at_ms INTEGER
);
CREATE TABLE transfer_steps (
  run_id TEXT NOT NULL REFERENCES transfer_runs(id) ON DELETE RESTRICT,
  ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
  locator_json TEXT NOT NULL CHECK(json_valid(locator_json)),
  phase TEXT NOT NULL,
  state TEXT NOT NULL CHECK(
    state IN ('not_sent', 'sent', 'applied', 'rolled_back', 'failed', 'skipped', 'outcome_unknown')
  ),
  digest TEXT NOT NULL CHECK(length(digest) = 64 AND digest NOT GLOB '*[^0-9a-f]*'),
  outcome_json TEXT NOT NULL CHECK(json_valid(outcome_json)),
  PRIMARY KEY(run_id, ordinal)
);
CREATE TABLE transfer_key_ops (
  run_id TEXT NOT NULL REFERENCES transfer_runs(id) ON DELETE RESTRICT,
  source_digest TEXT NOT NULL CHECK(length(source_digest) = 64 AND source_digest NOT GLOB '*[^0-9a-f]*'),
  target_digest TEXT NOT NULL CHECK(length(target_digest) = 64 AND target_digest NOT GLOB '*[^0-9a-f]*'),
  step_ordinal INTEGER NOT NULL CHECK(step_ordinal >= 0),
  PRIMARY KEY(run_id, source_digest),
  UNIQUE(run_id, target_digest),
  FOREIGN KEY(run_id, step_ordinal) REFERENCES transfer_steps(run_id, ordinal) ON DELETE RESTRICT
);
CREATE TABLE transfer_artifacts (
  id TEXT NOT NULL PRIMARY KEY,
  run_id TEXT REFERENCES transfer_runs(id) ON DELETE RESTRICT,
  owner_client_id TEXT,
  kind TEXT NOT NULL CHECK(kind IN ('export_partial', 'report', 'project_stage', 'redis_temporary', 'input_index')),
  state TEXT NOT NULL CHECK(
    state IN ('preparing', 'ready', 'published', 'cleanup_pending', 'cleaned', 'outcome_unknown', 'recovery_conflict')
  ),
  identity_json TEXT NOT NULL CHECK(json_valid(identity_json)),
  revision INTEGER NOT NULL CHECK(revision > 0)
);
CREATE TABLE project_bindings (
  project_id TEXT NOT NULL,
  binding_group TEXT NOT NULL,
  connection_alias TEXT NOT NULL,
  profile_id TEXT NOT NULL,
  database_json TEXT NOT NULL CHECK(json_valid(database_json)),
  revision INTEGER NOT NULL CHECK(revision > 0),
  PRIMARY KEY(project_id, binding_group, connection_alias)
);
CREATE INDEX transfer_runs_workspace_created ON transfer_runs(workspace_id, created_at_ms);
CREATE INDEX transfer_steps_run_state ON transfer_steps(run_id, state, ordinal);
CREATE INDEX transfer_steps_input_position ON transfer_steps(
  run_id,
  json_extract(locator_json, '$.input'),
  json_extract(locator_json, '$.byte_start'),
  phase
);
CREATE INDEX transfer_artifacts_state ON transfer_artifacts(state);
CREATE TABLE project_binding_confirmations (
  project_id TEXT NOT NULL,
  root TEXT NOT NULL,
  binding_group TEXT NOT NULL,
  binding_digest TEXT NOT NULL,
  PRIMARY KEY (project_id, root, binding_group)
);
PRAGMA user_version = 5;
