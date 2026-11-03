use super::*;

#[test]
fn export_never_replaces_an_existing_file() {
    let root = std::env::temp_dir().join(format!("datapier-export-test-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let tmp = root.join("partial");
    let dst = root.join("output");
    fs::write(&tmp, b"new").unwrap();
    fs::write(&dst, b"existing").unwrap();
    assert_eq!(publish_export(&tmp, &dst).unwrap_err().code, "conflict");
    assert_eq!(fs::read(&dst).unwrap(), b"existing");
    assert_eq!(fs::read(&tmp).unwrap(), b"new");
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn cached_results_preserve_origin_and_existing_direct_format() {
    let root = std::env::temp_dir().join(format!("datapier-result-export-{}", Uuid::new_v4()));
    let store = crate::Store::open(&root).await.unwrap();
    let sink = ExecutionSink::new(store.clone(), Uuid::new_v4());
    let id = sink
        .start(vec![Column::new("value", "text")], "query")
        .await
        .unwrap();
    sink.append(id, vec![vec![Value::Text("中".into())], vec![Value::Null]])
        .await
        .unwrap();
    let summary = json!({"origin":{"source":"archery","provider":"redis"},"coverage":{"scope":"returned_response"},"fidelity":{"kind":"platform_text"}});
    sink.finish(id, summary.clone()).await.unwrap();
    let cancel = CancellationToken::new();
    let json_path = root.join("query.json");
    export_cached(
        id,
        &sink,
        &cancel,
        &mut Progress::default(),
        "json",
        File::create(&json_path).unwrap(),
        false,
    )
    .await
    .unwrap();
    let doc: Json = serde_json::from_slice(&fs::read(json_path).unwrap()).unwrap();
    assert_eq!(doc["format"], "datapier-query-result");
    assert_eq!(doc["summary"], summary);
    assert_eq!(doc["rows"].as_array().unwrap().len(), 2);
    let csv_path = root.join("query.csv");
    export_cached(
        id,
        &sink,
        &cancel,
        &mut Progress::default(),
        "csv",
        File::create(&csv_path).unwrap(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(fs::read_to_string(csv_path).unwrap(), "value\n中\n\\N\n");
    sink.finish(id, json!({"origin":{"source":"direct"}}))
        .await
        .unwrap();
    let direct_path = root.join("direct.json");
    export_cached(
        id,
        &sink,
        &cancel,
        &mut Progress::default(),
        "json",
        File::create(&direct_path).unwrap(),
        false,
    )
    .await
    .unwrap();
    let doc: Json = serde_json::from_slice(&fs::read(direct_path).unwrap()).unwrap();
    assert_eq!(doc["format"], "datapier");
    assert!(doc.get("summary").is_none());
    drop(sink);
    store.pool.close().await;
    store.cache.close().await;
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn incomplete_archery_cache_is_not_exported() {
    let root = std::env::temp_dir().join(format!("datapier-incomplete-export-{}", Uuid::new_v4()));
    let store = crate::Store::open(&root).await.unwrap();
    let sink = ExecutionSink::new(store.clone(), Uuid::new_v4());
    let id = sink
        .start(vec![Column::new("value", "text")], "rows")
        .await
        .unwrap();
    sink.append(id, vec![vec![Value::Text("partial".into())]])
        .await
        .unwrap();
    sqlx::query("UPDATE results SET summary=? WHERE id=?")
        .bind(json!({"origin":{"source":"archery","provider":"mysql"}}).to_string())
        .bind(id.to_string())
        .execute(&store.cache)
        .await
        .unwrap();
    let path = root.join("partial");
    let error = export_cached(
        id,
        &sink,
        &CancellationToken::new(),
        &mut Progress::default(),
        "json",
        File::create(&path).unwrap(),
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "incomplete_result");
    assert!(fs::read(path).unwrap().is_empty());
    drop(sink);
    store.pool.close().await;
    store.cache.close().await;
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

struct FailingExportFile {
    file: File,
    remaining: Option<usize>,
    fail_flush: bool,
}
impl Write for FailingExportFile {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if let Some(remaining) = self.remaining.as_mut() {
            if *remaining == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    "injected disk full",
                ));
            }
            let count = self.file.write(&bytes[..bytes.len().min(*remaining)])?;
            *remaining -= count;
            Ok(count)
        } else {
            self.file.write(bytes)
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if self.fail_flush {
            Err(std::io::Error::other("injected flush failure"))
        } else {
            self.file.flush()
        }
    }
}
impl ExportFile for FailingExportFile {
    fn sync_all(&self) -> std::io::Result<()> {
        self.file.sync_all()
    }
}

#[tokio::test]
async fn cached_export_write_and_flush_failures_keep_partial_and_progress() {
    let root = std::env::temp_dir().join(format!("datapier-export-io-{}", Uuid::new_v4()));
    let store = crate::Store::open(&root).await.unwrap();
    let sink = ExecutionSink::new(store.clone(), Uuid::new_v4());
    let id = sink
        .start(vec![Column::new("value", "text")], "rows")
        .await
        .unwrap();
    sink.append(id, vec![vec![Value::Text("x".repeat(20000))]])
        .await
        .unwrap();
    sink.finish(id, json!({"origin":{"source":"archery"}}))
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    for fail_flush in [false, true] {
        let dst = root.join(if fail_flush {
            "flush.json"
        } else {
            "write.json"
        });
        let mut progress = Progress::default();
        let (tmp, file) = create_temp(&dst, &mut progress).unwrap();
        let file = FailingExportFile {
            file,
            remaining: if fail_flush { None } else { Some(128) },
            fail_flush,
        };
        let result = export_cached(id, &sink, &cancel, &mut progress, "json", file, false).await;
        let result = finish_export(result, &tmp, &dst, &cancel, &mut progress, "json");
        let error = with_progress(result, &progress).unwrap_err();
        assert!(!dst.exists());
        assert!(tmp.exists());
        let partial = fs::read(&tmp).unwrap();
        if fail_flush {
            let partial: Json = serde_json::from_slice(&partial).unwrap();
            assert_eq!(
                partial["rows"][0][0],
                json!({"type":"text","value":"x".repeat(20000)})
            );
        } else {
            assert_eq!(partial.len(), 128);
            assert!(serde_json::from_slice::<Json>(&partial).is_err());
        }
        assert_eq!(error.details["progress"]["partial_path"], json!(tmp));
        assert_eq!(
            error.details["progress"]["completed_rows"],
            if fail_flush { 1 } else { 0 }
        );
        assert_eq!(error.details["progress"]["remaining"], "not_executed");
    }
    drop(sink);
    store.pool.close().await;
    store.cache.close().await;
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn migrated_direct_cache_exports_without_rewriting_historical_parameters() {
    let root = std::env::temp_dir().join(format!("datapier-old-cache-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let task_id = Uuid::new_v4();
    let result_id = Uuid::new_v4();
    let historical = json!({"text":"SELECT 9007199254740993","_task_context":{
        "connection":"旧直连","provider":"mysql","database":"old_db","redis_db":null,"targets":[]
    }});
    let mut config = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(root.join("datapier.sqlite3"))
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0001_config.sql"))
        .execute(&mut config)
        .await
        .unwrap();
    sqlx::query("PRAGMA user_version=1")
        .execute(&mut config)
        .await
        .unwrap();
    sqlx::query("INSERT INTO executions(id,owner,workspace_id,method,params,state,summary) VALUES(?,?,'legacy','execution.start',?,'succeeded',?)")
            .bind(task_id.to_string()).bind(Uuid::new_v4().to_string()).bind(historical.to_string())
            .bind(json!({"results":[result_id]}).to_string()).execute(&mut config).await.unwrap();
    config.close().await.unwrap();
    let mut cache = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(root.join("cache.sqlite3"))
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0001_cache.sql"))
        .execute(&mut cache)
        .await
        .unwrap();
    sqlx::query("PRAGMA user_version=1")
        .execute(&mut cache)
        .await
        .unwrap();
    sqlx::query("INSERT INTO results(id,execution_id,columns,kind,row_count,complete,summary) VALUES(?,?,?,'rows',1,1,'null')")
            .bind(result_id.to_string()).bind(task_id.to_string())
            .bind(serde_json::to_string(&vec![Column::new("n","BIGINT")]).unwrap()).execute(&mut cache).await.unwrap();
    sqlx::query("INSERT INTO result_rows(result_id,row_no,payload) VALUES(?,0,?)")
        .bind(result_id.to_string())
        .bind(serde_json::to_vec(&vec![Value::Integer("9007199254740993".into())]).unwrap())
        .execute(&mut cache)
        .await
        .unwrap();
    cache.close().await.unwrap();

    let store = crate::Store::open(&root).await.unwrap();
    let params: String = sqlx::query_scalar("SELECT params FROM executions WHERE id=?")
        .bind(task_id.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(serde_json::from_str::<Json>(&params).unwrap(), historical);
    let sink = ExecutionSink::new(store.clone(), task_id);
    let path = root.join("legacy.json");
    export_cached(
        result_id,
        &sink,
        &CancellationToken::new(),
        &mut Progress::default(),
        "json",
        File::create(&path).unwrap(),
        false,
    )
    .await
    .unwrap();
    let output: Json = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(output["format"], "datapier");
    assert_eq!(
        output["rows"][0][0],
        json!({"type":"integer","value":"9007199254740993"})
    );

    // 新建任务没有迁移证据时，不能根据当前连接或数据库类型猜成 Direct。
    let unknown = store
        .create_task(
            Uuid::new_v4(),
            "legacy",
            "execution.start",
            historical,
            false,
        )
        .await
        .unwrap();
    let unknown_sink = ExecutionSink::new(store.clone(), unknown.id);
    let unknown_result = unknown_sink
        .start(vec![Column::new("n", "BIGINT")], "rows")
        .await
        .unwrap();
    unknown_sink
        .append(unknown_result, vec![vec![Value::Integer("1".into())]])
        .await
        .unwrap();
    unknown_sink
        .finish(unknown_result, Json::Null)
        .await
        .unwrap();
    assert!(
        export_cached(
            unknown_result,
            &unknown_sink,
            &CancellationToken::new(),
            &mut Progress::default(),
            "json",
            File::create(root.join("unknown.json")).unwrap(),
            false
        )
        .await
        .is_err()
    );
    drop(unknown_sink);
    drop(sink);
    store.pool.close().await;
    store.cache.close().await;
    drop(store);
    fs::remove_dir_all(root).unwrap();
}
