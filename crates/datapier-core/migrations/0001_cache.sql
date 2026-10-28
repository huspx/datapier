CREATE TABLE results (
    id TEXT PRIMARY KEY,
    execution_id TEXT NOT NULL,
    columns TEXT NOT NULL,
    kind TEXT NOT NULL,
    row_count INTEGER NOT NULL DEFAULT 0,
    complete INTEGER NOT NULL DEFAULT 0,
    summary TEXT NOT NULL DEFAULT 'null'
);
CREATE INDEX results_execution ON results(execution_id);
CREATE TABLE result_rows (
    result_id TEXT NOT NULL REFERENCES results(id) ON DELETE CASCADE,
    row_no INTEGER NOT NULL,
    payload BLOB NOT NULL,
    PRIMARY KEY(result_id,row_no)
) WITHOUT ROWID;
