//! 有界、流式持久化 SQL 输入索引；文件不包含语句正文或行值。

use super::{
    BLOCK, DatabaseContext, DeclaredObject, SqlIndex, SqlKind, SqlStatement, VersionConstraint,
};
use crate::model::{AppError, Result};
use serde::{Serialize, Serializer, ser::SerializeSeq};
use std::io::{self, Write};
use tokio_util::sync::CancellationToken;

pub(crate) const MAX_INDEX_DISK_BYTES: u64 = 64 * 1024 * 1024;

fn kind(kind: SqlKind) -> &'static str {
    match kind {
        SqlKind::Database => "database",
        SqlKind::Table => "table",
        SqlKind::View => "view",
        SqlKind::Trigger => "trigger",
        SqlKind::Procedure => "procedure",
        SqlKind::Function => "function",
        SqlKind::Event => "event",
        SqlKind::Index => "index",
        SqlKind::Drop => "drop",
        SqlKind::Insert => "insert",
        SqlKind::Context => "context",
        SqlKind::Wrapper => "wrapper",
    }
}

// 只显式列出安全的结构字段，避免将 settings 或 DDL 加入磁盘格式。
#[derive(Serialize)]
struct Context<'a> {
    database: &'a Option<String>,
    no_backslash_escapes: bool,
    ansi_quotes: bool,
    input_encoding: &'a str,
    character_set_client: &'a Option<String>,
}

#[derive(Serialize)]
struct DatabaseDefaults<'a> {
    database: &'a str,
    before_statement: usize,
    restore_statement: usize,
}
impl<'a> From<&'a DatabaseContext> for DatabaseDefaults<'a> {
    fn from(value: &'a DatabaseContext) -> Self {
        Self {
            database: &value.database,
            before_statement: value.before_statement,
            restore_statement: value.restore_statement,
        }
    }
}

#[derive(Serialize)]
struct Constraint {
    start: u64,
    end: u64,
    minimum: Option<u32>,
}
impl From<&VersionConstraint> for Constraint {
    fn from(value: &VersionConstraint) -> Self {
        Self {
            start: value.start,
            end: value.end,
            minimum: value.minimum,
        }
    }
}

struct Constraints<'a>(&'a [VersionConstraint]);
impl Serialize for Constraints<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for value in self.0 {
            seq.serialize_element(&Constraint::from(value))?;
        }
        seq.end()
    }
}

#[derive(Serialize)]
struct Statement<'a> {
    start: u64,
    end: u64,
    line: u64,
    kind: &'static str,
    is_create: bool,
    drop_kind: Option<&'static str>,
    object: &'a Option<String>,
    source_database: &'a Option<String>,
    source_name: &'a Option<String>,
    index_table_database: &'a Option<String>,
    index_table_name: &'a Option<String>,
    sql_security: &'a Option<String>,
    columns: &'a Option<Vec<String>>,
    source_columns: &'a Option<Vec<String>>,
    row_width: Option<usize>,
    context: Context<'a>,
    version: Option<u32>,
    version_constraints: Constraints<'a>,
    unresolved_references: bool,
    placeholder_view: bool,
    tuple_start: Option<u64>,
    references: &'a [String],
    function_calls: &'a [String],
    database_context: Option<DatabaseDefaults<'a>>,
    alters_database_defaults: bool,
}
impl<'a> From<&'a SqlStatement> for Statement<'a> {
    fn from(s: &'a SqlStatement) -> Self {
        Self {
            start: s.start,
            end: s.end,
            line: s.line,
            kind: kind(s.kind),
            is_create: s.is_create,
            drop_kind: s.drop_kind.map(kind),
            object: &s.object,
            source_database: &s.source_database,
            source_name: &s.source_name,
            index_table_database: &s.index_table_database,
            index_table_name: &s.index_table_name,
            sql_security: &s.sql_security,
            columns: &s.columns,
            source_columns: &s.source_columns,
            row_width: s.row_width,
            context: Context {
                database: &s.context.database,
                no_backslash_escapes: s.context.no_backslash_escapes,
                ansi_quotes: s.context.ansi_quotes,
                input_encoding: &s.context.input_encoding,
                character_set_client: &s.context.character_set_client,
            },
            version: s.version,
            version_constraints: Constraints(&s.version_constraints),
            unresolved_references: s.unresolved_references,
            placeholder_view: s.placeholder_view,
            tuple_start: s.tuple_start,
            references: &s.references,
            function_calls: &s.function_calls,
            database_context: s.database_context.as_ref().map(DatabaseDefaults::from),
            alters_database_defaults: s.alters_database_defaults,
        }
    }
}

struct Statements<'a>(&'a [SqlStatement]);
impl Serialize for Statements<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for statement in self.0 {
            seq.serialize_element(&Statement::from(statement))?;
        }
        seq.end()
    }
}

#[derive(Serialize)]
struct Declaration<'a> {
    kind: &'static str,
    database: &'a str,
    name: &'a str,
    schema: bool,
    data: bool,
}
struct Declarations<'a>(&'a [DeclaredObject]);
impl Serialize for Declarations<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for item in self.0 {
            seq.serialize_element(&Declaration {
                kind: kind(item.kind),
                database: &item.database,
                name: &item.name,
                schema: item.schema,
                data: item.data,
            })?;
        }
        seq.end()
    }
}

struct BlockHashes<'a>(&'a [[u8; 32]]);
impl Serialize for BlockHashes<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for hash in self.0 {
            let mut hex = [0u8; 64];
            for (i, byte) in hash.iter().enumerate() {
                hex[2 * i] = HEX[(byte >> 4) as usize];
                hex[2 * i + 1] = HEX[(byte & 15) as usize];
            }
            // HEX 中的字节都是 ASCII。
            seq.serialize_element(std::str::from_utf8(&hex).expect("hex is ASCII"))?;
        }
        seq.end()
    }
}

#[derive(Serialize)]
struct PersistedIndex<'a> {
    format: &'static str,
    version: u32,
    source_sha256: &'a str,
    source_bytes: u64,
    block_bytes: usize,
    block_sha256: BlockHashes<'a>,
    datapier_format: bool,
    input_encoding: &'a str,
    bom_bytes: usize,
    fixed_encoding: bool,
    declared_objects: Declarations<'a>,
    statements: Statements<'a>,
}

struct BoundedWriter<'a, W> {
    output: &'a mut W,
    cancel: &'a CancellationToken,
    remaining: &'a mut u64,
    error: Option<AppError>,
}
impl<W: Write> Write for BoundedWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            self.error = Some(AppError::cancelled());
            return Err(io::Error::other("SQL 索引写入已取消"));
        }
        if bytes.len() as u64 > *self.remaining {
            self.error = Some(AppError::new("resource_limit", "SQL 索引超过磁盘预算"));
            return Err(io::Error::other("SQL 索引超过磁盘预算"));
        }
        match self.output.write(bytes) {
            Ok(n) => {
                *self.remaining -= n as u64;
                Ok(n)
            }
            Err(error) => {
                if error.kind() != io::ErrorKind::Interrupted {
                    self.error = Some(AppError::new("io", error.to_string()));
                }
                Err(error)
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.cancel.is_cancelled() {
            self.error = Some(AppError::cancelled());
            return Err(io::Error::other("SQL 索引写入已取消"));
        }
        self.output.flush()
    }
}

/// 以 JSON v1 写入源文件身份、分块摘要和结构定位；`remaining` 由所有输入共享。
pub(crate) fn write_index<W: Write>(
    index: &SqlIndex,
    output: &mut W,
    cancel: &CancellationToken,
    remaining: &mut u64,
) -> Result<()> {
    let data = PersistedIndex {
        format: "datapier-sql-index",
        version: 1,
        source_sha256: &index.sha256,
        source_bytes: index.bytes,
        block_bytes: BLOCK,
        block_sha256: BlockHashes(&index.block_hashes),
        datapier_format: index.datapier_format,
        input_encoding: &index.input_encoding,
        bom_bytes: index.bom_bytes,
        fixed_encoding: index.fixed_encoding,
        declared_objects: Declarations(&index.declared_objects),
        statements: Statements(&index.statements),
    };
    let mut writer = BoundedWriter {
        output,
        cancel,
        remaining,
        error: None,
    };
    if let Err(error) = serde_json::to_writer(&mut writer, &data) {
        return Err(writer.error.take().unwrap_or_else(|| {
            if error.is_io() {
                AppError::new("io", error.to_string())
            } else {
                error.into()
            }
        }));
    }
    if cancel.is_cancelled() {
        return Err(AppError::cancelled());
    }
    writer
        .flush()
        .map_err(|error| writer.error.take().unwrap_or_else(|| AppError::from(error)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{fs, path::PathBuf};
    use uuid::Uuid;

    const SQL: &str = "CREATE TABLE `items` (`id` INT, `note` TEXT);\nINSERT INTO `items` (`id`, `note`) VALUES (1, 'private-row-literal');\n";

    struct Input(PathBuf);
    impl Input {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("datapier-index-{}.sql", Uuid::new_v4()));
            fs::write(&path, SQL).unwrap();
            Self(path)
        }
        fn index(&self) -> SqlIndex {
            super::super::inspect(&self.0).unwrap()
        }
    }
    impl Drop for Input {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn persists_only_structural_metadata_with_valid_offsets_and_hashes() {
        let input = Input::new();
        let index = input.index();
        let mut output = Vec::new();
        let mut remaining = MAX_INDEX_DISK_BYTES;
        write_index(
            &index,
            &mut output,
            &CancellationToken::new(),
            &mut remaining,
        )
        .unwrap();
        assert_eq!(remaining, MAX_INDEX_DISK_BYTES - output.len() as u64);
        let text = std::str::from_utf8(&output).unwrap();
        assert!(!text.contains("private-row-literal"));
        assert!(!text.contains("CREATE TABLE"));
        assert!(!text.contains("INSERT INTO"));
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["format"], "datapier-sql-index");
        assert_eq!(value["version"], 1);
        assert_eq!(value["source_bytes"], SQL.len() as u64);
        assert_eq!(
            value["source_sha256"],
            format!("{:x}", Sha256::digest(SQL.as_bytes()))
        );
        assert_eq!(value["block_bytes"], BLOCK as u64);
        assert_eq!(
            value["block_sha256"][0],
            format!("{:x}", Sha256::digest(SQL.as_bytes()))
        );
        let statements = value["statements"].as_array().unwrap();
        let insert = statements.iter().find(|s| s["kind"] == "insert").unwrap();
        let start = insert["start"].as_u64().unwrap() as usize;
        let end = insert["end"].as_u64().unwrap() as usize;
        assert!(SQL[start..end].contains("private-row-literal"));
        assert_eq!(insert["source_name"], "items");
    }

    #[test]
    fn quota_counts_actual_bytes_and_is_shared() {
        let input = Input::new();
        let index = input.index();
        let cancel = CancellationToken::new();
        let mut full = Vec::new();
        let mut unlimited = MAX_INDEX_DISK_BYTES;
        write_index(&index, &mut full, &cancel, &mut unlimited).unwrap();
        let mut remaining = full.len() as u64 * 2 - 1;
        let mut first = Vec::new();
        write_index(&index, &mut first, &cancel, &mut remaining).unwrap();
        let mut second = Vec::new();
        let error = write_index(&index, &mut second, &cancel, &mut remaining).unwrap_err();
        assert_eq!(error.code, "resource_limit");
        assert_eq!(remaining, full.len() as u64 - 1 - second.len() as u64);
        let mut exact = full.len() as u64;
        write_index(&index, &mut Vec::new(), &cancel, &mut exact).unwrap();
        assert_eq!(exact, 0);
    }

    struct ShortWriter {
        bytes: Vec<u8>,
        fail_after: Option<usize>,
        flush_fails: bool,
        cancel: Option<CancellationToken>,
    }
    impl Write for ShortWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self
                .fail_after
                .is_some_and(|limit| self.bytes.len() >= limit)
            {
                return Err(io::Error::other("write failure"));
            }
            let n = bytes.len().min(1);
            self.bytes.extend_from_slice(&bytes[..n]);
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.flush_fails {
                Err(io::Error::other("flush failure"))
            } else {
                Ok(())
            }
        }
    }
    fn short_writer() -> ShortWriter {
        ShortWriter {
            bytes: Vec::new(),
            fail_after: None,
            flush_fails: false,
            cancel: None,
        }
    }

    #[test]
    fn short_writes_and_io_failures_are_accounted_for() {
        let input = Input::new();
        let index = input.index();
        let cancel = CancellationToken::new();
        let mut writer = short_writer();
        let mut remaining = MAX_INDEX_DISK_BYTES;
        write_index(&index, &mut writer, &cancel, &mut remaining).unwrap();
        assert_eq!(remaining, MAX_INDEX_DISK_BYTES - writer.bytes.len() as u64);
        let mut writer = short_writer();
        writer.fail_after = Some(10);
        let mut remaining = MAX_INDEX_DISK_BYTES;
        let error = write_index(&index, &mut writer, &cancel, &mut remaining).unwrap_err();
        assert_eq!(error.code, "io");
        assert!(error.message.contains("write failure"));
        assert_eq!(remaining, MAX_INDEX_DISK_BYTES - 10);
        let mut writer = short_writer();
        writer.flush_fails = true;
        let mut remaining = MAX_INDEX_DISK_BYTES;
        let error = write_index(&index, &mut writer, &cancel, &mut remaining).unwrap_err();
        assert_eq!(error.code, "io");
        assert!(error.message.contains("flush failure"));
        assert_eq!(remaining, MAX_INDEX_DISK_BYTES - writer.bytes.len() as u64);
    }

    #[test]
    fn cancellation_is_observed_before_and_during_writes() {
        let input = Input::new();
        let index = input.index();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let mut remaining = MAX_INDEX_DISK_BYTES;
        assert_eq!(
            write_index(&index, &mut Vec::new(), &cancelled, &mut remaining)
                .unwrap_err()
                .code,
            "cancelled"
        );
        assert_eq!(remaining, MAX_INDEX_DISK_BYTES);
        let token = CancellationToken::new();
        let mut writer = short_writer();
        writer.cancel = Some(token.clone());
        let error = write_index(&index, &mut writer, &token, &mut remaining).unwrap_err();
        assert_eq!(error.code, "cancelled");
        assert_eq!(remaining, MAX_INDEX_DISK_BYTES - writer.bytes.len() as u64);
    }
}
