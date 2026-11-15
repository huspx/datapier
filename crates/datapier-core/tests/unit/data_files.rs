use super::*;

fn path() -> PathBuf {
    std::env::temp_dir().join(format!("datapier-file-test-{}", Uuid::new_v4()))
}

#[test]
fn verifies_each_delivered_block_and_seek() {
    let path = path();
    fs::write(&path, b"abcdefgh").unwrap();
    let (_, mut reader) = fingerprint_open(&path, 4, 1024).unwrap();
    let mut first = [0; 4];
    reader.read_exact(&mut first).unwrap();
    assert_eq!(&first, b"abcd");
    fs::write(&path, b"abcdEFGH").unwrap();
    assert!(reader.read_exact(&mut first).is_err());
    reader.seek(SeekFrom::Start(4)).unwrap();
    assert!(reader.read_exact(&mut first).is_err());
    fs::remove_file(path).unwrap();
}

#[test]
fn refuses_replaced_path_and_existing_output() {
    let path = path();
    fs::write(&path, b"original").unwrap();
    let (fingerprint, mut reader) = fingerprint_open(&path, 4, 1024).unwrap();
    let replacement = path.with_extension("replacement");
    fs::write(&replacement, b"original").unwrap();
    fs::remove_file(&path).unwrap();
    fs::rename(&replacement, &path).unwrap();
    let mut byte = [0];
    assert!(reader.read(&mut byte).is_err());
    assert!(VerifiedReader::open(&fingerprint).is_err());
    let mut output = AtomicOutput::create(&path).unwrap();
    output.write_all(b"new").unwrap();
    let partial = output.partial_path().to_owned();
    assert!(output.publish().is_err());
    assert_eq!(fs::read(&path).unwrap(), b"original");
    assert_eq!(fs::read(&partial).unwrap(), b"new");
    fs::remove_file(path).unwrap();
    fs::remove_file(partial).unwrap();
}

#[test]
fn utf16_split_blocks_preserve_quoted_newlines_and_offsets() {
    use crate::model::{ProviderKind, Value, transfer::TransferInput};
    let path = path();
    let text = "name,value\r\n\"中文\n😀\",hello\r\n";
    let mut bytes = vec![0xff, 0xfe];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(&path, bytes).unwrap();
    let input: TransferInput =
        serde_json::from_value(serde_json::json!({"path":path,"csv":{"encoding":"utf-16le"}}))
            .unwrap();
    let fingerprint = fingerprint(&path, 3, 4096).unwrap();
    let mut reader = RowReader::open(&input, &fingerprint, ProviderKind::Mysql).unwrap();
    assert!(reader.needs_confirmation());
    match reader.next_event().unwrap().unwrap() {
        RowEvent::MysqlRow {
            record,
            byte_offset,
            values,
        } => {
            assert_eq!(record, 1);
            assert_eq!(
                byte_offset,
                2 + "name,value\r\n".encode_utf16().count() as u64 * 2
            );
            assert_eq!(
                values,
                vec![Value::Text("中文\n😀".into()), Value::Text("hello".into())]
            );
        }
        _ => panic!("unexpected event"),
    }
    assert!(reader.next_event().unwrap().is_none());
    fs::remove_file(path).unwrap();
}

#[test]
fn json_metadata_after_rows_preserves_typed_values() {
    use crate::model::{ProviderKind, Value, transfer::TransferInput};
    let path = path();
    fs::write(&path, br#"{"rows":[[{"type":"unsigned","value":"18446744073709551615"},{"type":"bytes","value":"AP8="}]],"columns":[{"name":"n","type_name":"BIGINT"},{"name":"b","type_name":"BLOB"}],"version":1,"format":"datapier"}"#).unwrap();
    let input: TransferInput =
        serde_json::from_value(serde_json::json!({"path":path,"format":"json"})).unwrap();
    let fingerprint = fingerprint(&path, 7, 8192).unwrap();
    let mut reader = RowReader::open(&input, &fingerprint, ProviderKind::Mysql).unwrap();
    assert_eq!(reader.columns().len(), 2);
    match reader.next_event().unwrap().unwrap() {
        RowEvent::MysqlRow { values, .. } => assert_eq!(
            values,
            vec![
                Value::Unsigned("18446744073709551615".into()),
                Value::Bytes("AP8=".into())
            ]
        ),
        _ => panic!("unexpected event"),
    }
    assert!(reader.next_event().unwrap().is_none());
    fs::remove_file(path).unwrap();
}

#[test]
fn csv_writer_roundtrips_null_backslash_and_utf16() {
    use crate::model::{
        Column, ProviderKind, Value,
        transfer::{CsvOptions, TransferFormat, TransferInput},
    };
    let path = path();
    let options = CsvOptions {
        encoding: "utf-16be".into(),
        ..Default::default()
    };
    let columns = vec![Column::new("a", "TEXT"), Column::new("b", "TEXT")];
    let output = AtomicOutput::create(&path).unwrap();
    let mut writer = CsvJsonWriter::mysql(
        output,
        TransferFormat::Csv,
        options.clone(),
        columns.clone(),
    )
    .unwrap();
    writer
        .write_row(&[Value::Null, Value::Text("\\N\n中文😀".into())])
        .unwrap();
    writer.finish().unwrap();
    let input: TransferInput =
        serde_json::from_value(serde_json::json!({"path":path,"csv":options,"columns":columns}))
            .unwrap();
    let fingerprint = fingerprint(&path, 5, 8192).unwrap();
    let mut reader = RowReader::open(&input, &fingerprint, ProviderKind::Mysql).unwrap();
    match reader.next_event().unwrap().unwrap() {
        RowEvent::MysqlRow { values, .. } => {
            assert_eq!(values, vec![Value::Null, Value::Text("\\N\n中文😀".into())])
        }
        _ => panic!("unexpected event"),
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn redis_rows_before_header_stream_as_events() {
    use crate::model::{ProviderKind, Value, transfer::TransferInput};
    let path = path();
    fs::write(&path, br#"{"records":[{"rows":[[{"type":"bytes","value":"AQ=="}]],"expires_at_ms":null,"type":"string","key_b64":"aw=="}],"version":2,"format":"datapier"}"#).unwrap();
    let input: TransferInput =
        serde_json::from_value(serde_json::json!({"path":path,"format":"json"})).unwrap();
    let fingerprint = fingerprint(&path, 8, 8192).unwrap();
    let mut reader = RowReader::open(&input, &fingerprint, ProviderKind::Redis).unwrap();
    assert!(
        matches!(reader.next_event().unwrap(), Some(RowEvent::RedisKeyHeader { key_b64, .. }) if key_b64 == "aw==")
    );
    assert!(
        matches!(reader.next_event().unwrap(), Some(RowEvent::RedisChunk { values, .. }) if values == vec![Value::Bytes("AQ==".into())])
    );
    assert!(matches!(
        reader.next_event().unwrap(),
        Some(RowEvent::RedisKeyEnd { record: 1 })
    ));
    assert!(reader.next_event().unwrap().is_none());
    fs::remove_file(path).unwrap();
}

#[test]
fn csv_conversion_error_continues_only_at_record_boundary() {
    use crate::model::{Column, ProviderKind, Value, transfer::TransferInput};
    let path = path();
    fs::write(&path, b"n\nnot-an-integer\n42\n").unwrap();
    let columns = vec![Column::new("n", "INT")];
    let input: TransferInput =
        serde_json::from_value(serde_json::json!({"path":path,"columns":columns})).unwrap();
    let fingerprint = fingerprint(&path, 8, 4096).unwrap();
    let mut reader = RowReader::open(&input, &fingerprint, ProviderKind::Mysql).unwrap();
    let error = reader.next_event().unwrap_err();
    assert_eq!(error.code, "conversion");
    assert_eq!(error.details["record"], 1);
    assert_eq!(error.details["column"], "n");
    assert_eq!(error.details["recoverable"], true);
    assert!(
        matches!(reader.next_event().unwrap(), Some(RowEvent::MysqlRow { record: 2, values, .. }) if values == vec![Value::Integer("42".into())])
    );
    fs::remove_file(path).unwrap();
}

#[test]
fn invalid_encoding_stops_at_original_byte_offset() {
    use crate::model::{ProviderKind, transfer::TransferInput};
    let path = path();
    fs::write(&path, b"name\n\xff\n").unwrap();
    let input: TransferInput = serde_json::from_value(serde_json::json!({"path":path})).unwrap();
    let fingerprint = fingerprint(&path, 2, 4096).unwrap();
    let mut reader = RowReader::open(&input, &fingerprint, ProviderKind::Mysql).unwrap();
    let error = reader.next_event().unwrap_err();
    assert_eq!(error.details["byte_offset"], 5);
    assert_ne!(error.code, "conversion");
    assert!(reader.next_event().is_err());
    fs::remove_file(path).unwrap();
}

#[test]
fn cancelled_scans_stop_before_open_and_iteration() {
    use crate::model::{ProviderKind, transfer::TransferInput};
    let path = path();
    fs::write(&path, b"a\n1\n").unwrap();
    let input: TransferInput = serde_json::from_value(serde_json::json!({"path":path})).unwrap();
    let fingerprint = fingerprint(&path, 4, 4096).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut reader =
        RowReader::open_with_cancel(&input, &fingerprint, ProviderKind::Mysql, &cancel).unwrap();
    cancel.cancel();
    assert_eq!(reader.next_event().unwrap_err().code, "cancelled");
    assert_eq!(
        fingerprint_with_cancel(&path, 4, 4096, &cancel)
            .unwrap_err()
            .code,
        "cancelled"
    );
    fs::remove_file(path).unwrap();
}

#[test]
fn gbk_and_gb18030_csv_roundtrip_without_replacement() {
    use crate::model::{
        Column, ProviderKind, Value,
        transfer::{CsvOptions, TransferFormat, TransferInput},
    };
    for (encoding, text) in [("gbk", "中文"), ("gb18030", "中文😀")] {
        let path = path();
        let options = CsvOptions {
            encoding: encoding.into(),
            ..Default::default()
        };
        let columns = vec![Column::new("v", "TEXT")];
        let mut writer = CsvJsonWriter::mysql(
            AtomicOutput::create(&path).unwrap(),
            TransferFormat::Csv,
            options.clone(),
            columns.clone(),
        )
        .unwrap();
        writer.write_row(&[Value::Text(text.into())]).unwrap();
        writer.finish().unwrap();
        let input: TransferInput = serde_json::from_value(
            serde_json::json!({"path":path,"csv":options,"columns":columns}),
        )
        .unwrap();
        let fingerprint = fingerprint(&path, 4, 8192).unwrap();
        let mut reader = RowReader::open(&input, &fingerprint, ProviderKind::Mysql).unwrap();
        assert!(
            matches!(reader.next_event().unwrap(), Some(RowEvent::MysqlRow { values, .. }) if values == vec![Value::Text(text.into())])
        );
        fs::remove_file(path).unwrap();
    }
    let path = path();
    let options = CsvOptions {
        encoding: "gbk".into(),
        ..Default::default()
    };
    let mut writer = CsvJsonWriter::mysql(
        AtomicOutput::create(&path).unwrap(),
        TransferFormat::Csv,
        options,
        vec![Column::new("v", "TEXT")],
    )
    .unwrap();
    let partial = writer.partial_path().to_owned();
    assert!(writer.write_row(&[Value::Text("😀".into())]).is_err());
    drop(writer);
    fs::remove_file(partial).unwrap();
}

#[test]
fn unclosed_csv_quote_is_not_a_recoverable_conversion() {
    use crate::model::{ProviderKind, transfer::TransferInput};
    let path = path();
    fs::write(&path, b"v\n\"broken\n42\n").unwrap();
    let input: TransferInput = serde_json::from_value(serde_json::json!({"path":path})).unwrap();
    let fingerprint = fingerprint(&path, 4, 4096).unwrap();
    let mut reader = RowReader::open(&input, &fingerprint, ProviderKind::Mysql).unwrap();
    let error = reader.next_event().unwrap_err();
    assert_ne!(error.code, "conversion");
    assert_eq!(error.details["record"], 1);
    assert_eq!(error.details["line"], 2);
    assert!(reader.next_event().is_err());
    fs::remove_file(path).unwrap();
}

#[test]
fn sealed_json_snapshot_matches_published_bytes() {
    use crate::model::{
        Column, Value,
        transfer::{CsvOptions, TransferFormat},
    };
    let path = path();
    let mut output = AtomicOutput::create(&path).unwrap();
    let reserved = output.snapshot(false).unwrap();
    assert_eq!(reserved.size, 0);
    let mut writer = CsvJsonWriter::mysql(
        output,
        TransferFormat::Json,
        CsvOptions::default(),
        vec![Column::new("v", "TEXT")],
    )
    .unwrap();
    writer.write_row(&[Value::Text("中文".into())]).unwrap();
    writer.seal().unwrap();
    let complete = writer.snapshot(true).unwrap();
    writer.finish().unwrap();
    let published = output_snapshot(&path, true).unwrap();
    assert_eq!(complete.file_id, published.file_id);
    assert_eq!(complete.size, published.size);
    assert_eq!(complete.sha256, published.sha256);
    fs::remove_file(path).unwrap();
}

#[test]
fn raw_record_reread_preserves_multiline_and_rechecks_input() {
    use crate::model::{ProviderKind, transfer::TransferInput};
    let path = path();
    fs::write(&path, b"a,b\n\"first\nline\",bad\nnext,ok\n").unwrap();
    let input: TransferInput = serde_json::from_value(serde_json::json!({"path":path})).unwrap();
    let fingerprint = fingerprint(&path, 4, 8192).unwrap();
    assert_eq!(
        read_raw_record(&input, &fingerprint, ProviderKind::Mysql, 1).unwrap(),
        b"\"first\nline\",bad\n"
    );
    fs::write(&path, b"a,b\n\"first\nline\",BAD\nnext,ok\n").unwrap();
    assert!(read_raw_record(&input, &fingerprint, ProviderKind::Mysql, 1).is_err());
    fs::remove_file(path).unwrap();
}

#[test]
fn json_row_limit_accepts_boundary_and_rejects_oversized_record() {
    use crate::model::{Column, Value, transfer::MAX_VALUE_BYTES};
    let path = path();
    let empty = vec![Value::Text(String::new())];
    let overhead = crate::execution::migration::encoded_size(&empty).unwrap();
    let exact = vec![Value::Text("a".repeat(MAX_VALUE_BYTES - overhead))];
    let mut writer = JsonWriter::mysql(
        AtomicOutput::create(&path).unwrap(),
        &[Column::new("v", "TEXT")],
    )
    .unwrap();
    writer.write_row(&exact).unwrap();
    writer.finish().unwrap();
    fs::remove_file(&path).unwrap();

    let mut writer = JsonWriter::mysql(
        AtomicOutput::create(&path).unwrap(),
        &[Column::new("a", "TEXT"), Column::new("b", "TEXT")],
    )
    .unwrap();
    let partial = writer.partial_path().to_owned();
    let too_large = vec![
        Value::Text("a".repeat(9 * 1024 * 1024)),
        Value::Text("b".repeat(9 * 1024 * 1024)),
    ];
    assert_eq!(
        writer.write_row(&too_large).unwrap_err().code,
        "resource_limit"
    );
    let error = writer.finish().unwrap_err();
    assert_eq!(publication_state(&error), PublicationState::NotPublished);
    assert!(!path.exists());
    assert!(partial.exists());
    fs::remove_file(partial).unwrap();
}

#[test]
fn prepublication_identity_failure_is_not_published() {
    let path = path();
    let output = AtomicOutput::create(&path).unwrap();
    let partial = output.partial_path().to_owned();
    fs::remove_file(&partial).unwrap();
    let error = output.publish().unwrap_err();
    assert_eq!(publication_state(&error), PublicationState::NotPublished);
    assert!(!path.exists());
}

#[test]
fn redis_hash_field_expiry_roundtrips_csv_and_json() {
    use crate::model::{
        ProviderKind, Value,
        transfer::{CsvOptions, TransferFormat, TransferInput},
    };
    for format in [TransferFormat::Csv, TransferFormat::Json] {
        let path = path();
        let mut writer = CsvJsonWriter::redis(
            AtomicOutput::create(&path).unwrap(),
            format,
            CsvOptions::default(),
        )
        .unwrap();
        writer
            .write_event(&RowEvent::RedisKeyHeader {
                record: 1,
                byte_offset: 0,
                key_b64: "a2V5".into(),
                kind: "hash".into(),
                expires_at_ms: serde_json::Value::Null,
            })
            .unwrap();
        let rows = vec![
            vec![
                Value::Bytes("b25l".into()),
                Value::Bytes("AP8=".into()),
                Value::Integer("4102444800000".into()),
            ],
            vec![
                Value::Bytes("dHdv".into()),
                Value::Bytes("".into()),
                Value::Integer("-1".into()),
            ],
            vec![
                Value::Bytes("dGhyZWU=".into()),
                Value::Bytes("dmFsdWU=".into()),
                Value::Integer("-1".into()),
            ],
        ];
        for row in &rows {
            writer
                .write_event(&RowEvent::RedisChunk {
                    record: 1,
                    byte_offset: 0,
                    values: row.clone(),
                })
                .unwrap();
        }
        writer
            .write_event(&RowEvent::RedisKeyEnd { record: 1 })
            .unwrap();
        writer.finish().unwrap();
        let input: TransferInput =
            serde_json::from_value(serde_json::json!({"path": path, "format": format})).unwrap();
        let fingerprint = fingerprint(&path, 64, 8192).unwrap();
        let raw = read_raw_record(&input, &fingerprint, ProviderKind::Redis, 1).unwrap();
        assert!(!raw.is_empty());
        let mut reader = RowReader::open(&input, &fingerprint, ProviderKind::Redis).unwrap();
        let mut actual = Vec::new();
        while let Some(event) = reader.next_event().unwrap() {
            if let RowEvent::RedisChunk { values, .. } = event {
                actual.push(values);
            }
        }
        assert_eq!(actual, rows);
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn legacy_redis_hash_csv_is_rejected() {
    use crate::model::{ProviderKind, transfer::TransferInput};
    let path = path();
    fs::write(&path, "version,record,key_b64,type,expires_at_ms,field_b64,index,member_b64,score,value_b64\n1,key,a2V5,hash,permanent,,,,,\n1,item,a2V5,hash,,ZmllbGQ=,,,,dmFsdWU=\n").unwrap();
    let input: TransferInput = serde_json::from_value(serde_json::json!({"path": path})).unwrap();
    let fingerprint = fingerprint(&path, 64, 8192).unwrap();
    assert!(RowReader::open(&input, &fingerprint, ProviderKind::Redis).is_err());
    fs::remove_file(path).unwrap();
}

#[test]
fn redis_json_rejects_v1_and_hash_without_expiry() {
    use crate::model::{ProviderKind, transfer::TransferInput};
    let path = path();
    let record = r#"{"key_b64":"aw==","type":"hash","expires_at_ms":null,"rows":[[{"type":"bytes","value":"Zg=="},{"type":"bytes","value":"dg=="}]]}"#;
    for version in [1, 2] {
        fs::write(
            &path,
            format!(r#"{{"format":"datapier","version":{version},"records":[{record}]}}"#),
        )
        .unwrap();
        let input: TransferInput =
            serde_json::from_value(serde_json::json!({"path":path,"format":"json"})).unwrap();
        let fingerprint = fingerprint(&path, 64, 8192).unwrap();
        if version == 1 {
            assert!(RowReader::open(&input, &fingerprint, ProviderKind::Redis).is_err());
            assert!(read_raw_record(&input, &fingerprint, ProviderKind::Redis, 1).is_err());
        } else {
            let mut reader = RowReader::open(&input, &fingerprint, ProviderKind::Redis).unwrap();
            assert!(reader.next_event().unwrap().is_some());
            assert!(reader.next_event().is_err());
        }
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn redis_csv_rejects_invalid_field_expiry_before_export() {
    use crate::model::{
        Value,
        transfer::{CsvOptions, TransferFormat},
    };
    let path = path();
    let mut writer = CsvJsonWriter::redis(
        AtomicOutput::create(&path).unwrap(),
        TransferFormat::Csv,
        CsvOptions::default(),
    )
    .unwrap();
    let partial = writer.partial_path().to_owned();
    writer
        .write_event(&RowEvent::RedisKeyHeader {
            record: 1,
            byte_offset: 0,
            key_b64: "a2V5".into(),
            kind: "hash".into(),
            expires_at_ms: serde_json::Value::Null,
        })
        .unwrap();
    let error = writer
        .write_event(&RowEvent::RedisChunk {
            record: 1,
            byte_offset: 0,
            values: vec![
                Value::Bytes("ZmllbGQ=".into()),
                Value::Bytes("dmFsdWU=".into()),
                Value::Integer("-2".into()),
            ],
        })
        .unwrap_err();
    assert_eq!(error.code, "invalid_input");
    assert!(!path.exists());
    drop(writer);
    fs::remove_file(partial).unwrap();
}
