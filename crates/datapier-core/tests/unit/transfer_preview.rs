use super::*;

#[test]
fn redis_csv_preview_rejects_invalid_key_contents() {
    let path = std::env::temp_dir().join(format!("datapier-preview-{}", Uuid::new_v4()));
    let target = ResourceTarget {
        database: None,
        name: String::new(),
        key_b64: None,
        kind: "database".into(),
    };
    let header =
        "version,record,key_b64,type,expires_at_ms,field_b64,index,member_b64,score,value_b64\n";
    for records in [
        "1,key,aw==,string,permanent,,,,,\n",
        "1,key,aw==,string,permanent,,,,,\n1,item,aw==,string,,,,,,YQ==\n1,item,aw==,string,,,,,,Yg==\n",
        "1,key,aw==,list,permanent,,,,,\n1,item,aw==,list,,,1,,,YQ==\n",
    ] {
        fs::write(&path, format!("{header}{records}")).unwrap();
        let preview = preview_redis_csv(
            &target,
            &CancellationToken::new(),
            File::open(&path).unwrap(),
        )
        .unwrap();
        assert!(!preview.complete);
        assert!(!preview.errors.is_empty());
    }
    fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn redis_score_validation_matches_csv_and_json_previews() {
    let root = std::env::temp_dir().join(format!("datapier-score-{}", Uuid::new_v4()));
    let store = crate::Store::open(&root).await.unwrap();
    let sink = ExecutionSink::new(store.clone(), Uuid::new_v4());
    let target = ResourceTarget {
        database: None,
        name: String::new(),
        key_b64: None,
        kind: "database".into(),
    };
    let path = root.join("scores");
    let cancel = CancellationToken::new();
    for (score, valid) in [
        ("1.25", true),
        ("+inf", true),
        ("-inf", true),
        ("NaN", false),
        ("1e999", false),
        ("bad", false),
    ] {
        let values = vec![Value::Bytes("YQ==".into()), Value::Float(score.into())];
        assert_eq!(redis_csv_row("aw==", "zset", &values).is_ok(), valid);
        let csv = format!(
            "version,record,key_b64,type,expires_at_ms,field_b64,index,member_b64,score,value_b64\n1,key,aw==,zset,permanent,,,,,\n1,item,aw==,zset,,,,YQ==,{score},\n"
        );
        fs::write(&path, csv).unwrap();
        let preview = preview_redis_csv(&target, &cancel, File::open(&path).unwrap()).unwrap();
        assert_eq!(preview.errors.is_empty(), valid, "CSV {score}");
        let doc = json!({"format":"datapier","version":1,"records":[{"key_b64":"aw==","type":"zset","expires_at_ms":null,"rows":[values]}]});
        fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
        let preview =
            preview_redis_json(&target, &sink, &cancel, File::open(&path).unwrap()).unwrap();
        assert_eq!(preview.errors.is_empty(), valid, "JSON {score}");
    }
    store.pool.close().await;
    store.cache.close().await;
    fs::remove_dir_all(root).unwrap();
}
