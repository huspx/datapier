CREATE TABLE profiles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    payload TEXT NOT NULL,
    credential_saved INTEGER NOT NULL DEFAULT 0 CHECK(credential_saved IN (0, 1))
);
CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    revision INTEGER NOT NULL CHECK(revision > 0),
    payload TEXT NOT NULL
);
CREATE TABLE executions (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    method TEXT NOT NULL,
    params TEXT NOT NULL,
    state TEXT NOT NULL CHECK(
        state IN ('queued', 'running', 'cancelling', 'succeeded', 'failed', 'cancelled', 'outcome_unknown')
    ),
    sent INTEGER NOT NULL DEFAULT 0 CHECK(sent IN (0, 1)),
    may_write INTEGER NOT NULL DEFAULT 0 CHECK(may_write IN (0, 1)),
    summary TEXT NOT NULL DEFAULT 'null',
    error TEXT,
    created_at REAL NOT NULL DEFAULT (unixepoch('subsec')),
    updated_at REAL NOT NULL DEFAULT (unixepoch('subsec'))
);
CREATE INDEX executions_workspace ON executions(workspace_id, created_at);
CREATE TABLE app_settings (
    id INTEGER PRIMARY KEY CHECK(id = 1),
    payload TEXT NOT NULL
);
INSERT INTO app_settings(id, payload)
VALUES (1, '{"language":"system"}');
