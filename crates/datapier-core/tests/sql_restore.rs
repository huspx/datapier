use datapier_core::{
    execution::sql_restore::{
        SqlKind, backup_footer, backup_header, encode_literal, inspect, read_inserts,
    },
    model::Value,
};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};
use uuid::Uuid;

struct Input(PathBuf);
impl Input {
    fn new(data: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!("datapier-sql-{}.sql", Uuid::new_v4()));
        fs::write(&path, data).unwrap();
        Self(path)
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn backup_digest_and_streamed_values_round_trip() {
    let text = "引号 ' 反斜线 \\\n";
    let lit = encode_literal(&Value::Text(text.into())).unwrap();
    let body = format!(
        "{}-- datapier-objects count=1\n-- datapier-object kind=table database=74657374 name=74 schema=1 data=1\nUSE `test`;\nCREATE TABLE `t` (`a` text);\nINSERT INTO `t` VALUES ({lit}),('two');\n",
        backup_header()
    );
    let footer = backup_footer(&Sha256::digest(body.as_bytes()));
    let file = Input::new(format!("{body}{footer}").as_bytes());
    let index = inspect(&file.0).unwrap();
    assert!(index.datapier_format);
    assert_eq!(
        index
            .statements
            .iter()
            .filter(|s| s.kind == SqlKind::Insert)
            .count(),
        1
    );
    let mut rows = Vec::new();
    read_inserts(&file.0, &index, "t", |_, row, _| {
        rows.push(row);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Text(text.into())],
            vec![Value::Text("two".into())]
        ]
    );
}

#[test]
fn rejects_unsafe_script_and_bad_footer_before_rows() {
    let file = Input::new(b"INSERT INTO t VALUES (1);\nSET @@GLOBAL.GTID_PURGED='x';\n");
    assert!(inspect(&file.0).is_err());
    let file = Input::new(
        format!(
            "{}INSERT INTO t VALUES (1);\n-- datapier-sql complete sha256={}\n",
            backup_header(),
            "0".repeat(64)
        )
        .as_bytes(),
    );
    assert!(inspect(&file.0).is_err());
}

#[test]
fn input_modification_invalidates_reader() {
    let file = Input::new(b"INSERT INTO t VALUES (1);\n");
    let index = inspect(&file.0).unwrap();
    fs::write(&file.0, b"INSERT INTO t VALUES (2);\n").unwrap();
    assert!(read_inserts(&file.0, &index, "t", |_, _, _| Ok(())).is_err());
}

#[test]
fn delimiter_version_comments_and_database_context() {
    let file = Input::new(b"USE `db`;\nALTER DATABASE `db` CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;\nDELIMITER ;;\n/*!50003 CREATE*/ /*!50003 PROCEDURE `p`() BEGIN SELECT 1; END */;;\nDELIMITER ;\nALTER DATABASE `db` CHARACTER SET latin1 COLLATE latin1_bin;\n");
    let index = inspect(&file.0).unwrap();
    let routine = index
        .statements
        .iter()
        .find(|s| s.kind == SqlKind::Procedure)
        .unwrap();
    assert!(routine.database_context.is_some());
    assert_eq!(routine.version, Some(50003));
}

#[test]
fn view_mapping_preserves_strings_and_security() {
    use datapier_core::{
        execution::sql_restore::{read_ddl, rewrite_ddl},
        model::transfer::{DefinerMapping, TransferObject},
    };
    let file = Input::new(b"USE `db`;\nCREATE DEFINER=`u`@`h` SQL SECURITY DEFINER VIEW `v` AS SELECT `db`.`t`.`id`, 'db.t' FROM `db`.`t`;\n");
    let index = inspect(&file.0).unwrap();
    let (number, statement) = index
        .statements
        .iter()
        .enumerate()
        .find(|(_, s)| s.kind == SqlKind::View)
        .unwrap();
    let mappings: Vec<TransferObject> = serde_json::from_value(serde_json::json!([
        {"source":{"database":"db","name":"v"},"target":{"database":"target","name":"v"}},
        {"source":{"database":"db","name":"t"},"target":{"database":"target","name":"t"}}
    ]))
    .unwrap();
    let result = rewrite_ddl(
        &read_ddl(&file.0, &index, number).unwrap(),
        statement,
        &mappings,
        &DefinerMapping::CurrentUser,
    )
    .unwrap();
    assert!(result.contains("DEFINER=CURRENT_USER SQL SECURITY DEFINER"));
    assert!(result.contains("`target`.`t`.`id`"));
    assert!(result.contains("'db.t'"));
}

#[test]
fn empty_backup_requires_explicit_object_count() {
    let body = format!(
        "{}-- datapier-objects count=0\nCREATE DATABASE `empty_db`;\n",
        backup_header()
    );
    let file =
        Input::new(format!("{body}{}", backup_footer(&Sha256::digest(body.as_bytes()))).as_bytes());
    assert!(inspect(&file.0).unwrap().declared_objects.is_empty());
    let body = format!("{}CREATE DATABASE `empty_db`;\n", backup_header());
    let file =
        Input::new(format!("{body}{}", backup_footer(&Sha256::digest(body.as_bytes()))).as_bytes());
    assert!(inspect(&file.0).is_err());
}

#[test]
fn indexes_function_dependency_before_definition() {
    let file = Input::new(b"USE `db`;\nCREATE VIEW `v` AS SELECT `db`.`f`(1);\nDELIMITER ;;\nCREATE FUNCTION `f`(x INT) RETURNS INT DETERMINISTIC RETURN x;;\nDELIMITER ;\n");
    let index = inspect(&file.0).unwrap();
    let view = index
        .statements
        .iter()
        .find(|s| s.kind == SqlKind::View)
        .unwrap();
    assert!(view.function_calls.iter().any(|s| s == "db.f"));
    assert!(view.references.iter().any(|s| s == "db.f"));
    assert!(!view.unresolved_references);
}

#[test]
fn latin1_and_gbk_are_decoded_with_source_byte_offsets() {
    let mut latin = b"SET NAMES latin1;\nINSERT INTO t VALUES ('".to_vec();
    latin.extend_from_slice(&[0x80, b'\'', b')', b';', b'\n']);
    let file = Input::new(&latin);
    let index = inspect(&file.0).unwrap();
    let mut rows = Vec::new();
    read_inserts(&file.0, &index, "t", |_, row, offset| {
        rows.push((row, offset));
        Ok(())
    })
    .unwrap();
    assert_eq!(rows[0].0, vec![Value::Text("€".into())]);
    assert_eq!(latin[rows[0].1 as usize], b'(');

    let sql = "SET NAMES gbk;\nINSERT INTO t VALUES ('你好');\n";
    let (encoded, _, errors) = encoding_rs::GBK.encode(sql);
    assert!(!errors);
    let file = Input::new(&encoded);
    let index = inspect(&file.0).unwrap();
    let mut rows = Vec::new();
    read_inserts(&file.0, &index, "t", |_, row, _| {
        rows.push(row);
        Ok(())
    })
    .unwrap();
    assert_eq!(rows, vec![vec![Value::Text("你好".into())]]);
}

#[test]
fn utf16_bom_and_malformed_encoding_are_checked() {
    let sql = "INSERT INTO t VALUES ('中文');\n";
    let mut bytes = vec![0xff, 0xfe];
    for unit in sql.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let file = Input::new(&bytes);
    let index = inspect(&file.0).unwrap();
    let mut rows = Vec::new();
    read_inserts(&file.0, &index, "t", |_, row, _| {
        rows.push(row);
        Ok(())
    })
    .unwrap();
    assert_eq!(rows, vec![vec![Value::Text("中文".into())]]);
    let file = Input::new(b"INSERT INTO t VALUES ('\xff');\n");
    assert!(inspect(&file.0).is_err());
}
