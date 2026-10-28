CREATE TABLE value_files (
    id TEXT PRIMARY KEY,
    result_id TEXT NOT NULL
);
CREATE INDEX value_files_result ON value_files(result_id);
