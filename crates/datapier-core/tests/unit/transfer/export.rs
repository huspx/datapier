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
    let task = store
        .create_task(Uuid::new_v4(), "test", "execution.start", json!({}), false)
        .await
        .unwrap();
    let sink = ExecutionSink::new(store.clone(), task.id);
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
    assert_eq!(doc["summary"]["origin"]["source"], "direct");
    drop(sink);
    store.pool.close().await;
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn incomplete_archery_exports_retained_rows() {
    let root = std::env::temp_dir().join(format!("datapier-incomplete-export-{}", Uuid::new_v4()));
    let store = crate::Store::open(&root).await.unwrap();
    let task = store
        .create_task(Uuid::new_v4(), "test", "execution.start", json!({}), false)
        .await
        .unwrap();
    let sink = ExecutionSink::new(store.clone(), task.id);
    let id = sink
        .start(vec![Column::new("value", "text")], "rows")
        .await
        .unwrap();
    sink.append(id, vec![vec![Value::Text("partial".into())]])
        .await
        .unwrap();
    store
        .update_result_summary(
            id,
            json!({"origin":{"source":"archery","provider":"mysql"},"resource_limited":true}),
        )
        .await
        .unwrap();
    let path = root.join("partial");
    export_cached(
        id,
        &sink,
        &CancellationToken::new(),
        &mut Progress::default(),
        "json",
        File::create(&path).unwrap(),
        false,
    )
    .await
    .unwrap();
    let doc: Json = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(doc["summary"]["resource_limited"], true);
    assert_eq!(doc["rows"].as_array().unwrap().len(), 1);
    drop(sink);
    store.pool.close().await;
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
    let task = store
        .create_task(Uuid::new_v4(), "test", "execution.start", json!({}), false)
        .await
        .unwrap();
    let sink = ExecutionSink::new(store.clone(), task.id);
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
    drop(store);
    fs::remove_dir_all(root).unwrap();
}
