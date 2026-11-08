use super::*;

#[test]
fn restored_value_accounts_for_json_nodes_and_keeps_pending_on_rejection() {
    for dense in [false, true] {
        let (backend, _requests, _) = Backend::scripted();
        let mut app = App::new("restore-budget".into());
        app.settings.limits.result_memory_bytes_per_client = 1024 * 1024;
        let body = if dense {
            json!(vec![0; 8192])
        } else {
            json!({"text":"中文\"\\", "values":[1,null,true]})
        };
        let expected = DataValue::Json(body);
        let raw = serde_json::to_vec(&expected).unwrap();
        let id = Uuid::new_v4();
        let original = DataValue::Large {
            id,
            original_type: "json".into(),
            bytes: raw.len() as u64,
            preview: "".into(),
        };
        let pending = json!({"rows":[{"operation":"update","values":{"v":{"type":"text","value":"用户新值"}}}]});
        app.restore_rows = Some(RowRestore {
            pending: pending.clone(),
            session: Value::Null,
            token: Value::Null,
            target: Value::Null,
            profile: Uuid::nil(),
            profile_revision: 0,
            columns: vec!["v".into()],
            draft_revision: 0,
            rows: vec![],
            files: vec![],
            staged: vec![],
            index: 0,
            task: None,
            result: Some(Uuid::new_v4()),
            before: Some([("v".into(), original.clone())].into_iter().collect()),
            chunk: None,
            in_flight: None,
            bytes: 0,
        });
        app.workspace_restore_value_or_release(&backend);
        let mut rejected = None;
        let chunk_bytes = if dense { 1024 } else { 3 };
        for (index, bytes) in raw.chunks(chunk_bytes).enumerate() {
            let offset = index * chunk_bytes;
            let end = offset + bytes.len();
            let response = json!({"offset":offset,"total_bytes":raw.len(),"data_b64":STANDARD.encode(bytes),"next":(end < raw.len()).then_some(end)});
            if let Err(error) = app.workspace_restore_accept(RestoreOp::Value, &response) {
                rejected = Some(error);
                break;
            }
        }
        let state = app.restore_rows.as_ref().unwrap();
        assert_eq!(state.pending, pending);
        if dense {
            assert_eq!(rejected.unwrap().code, "resource_limit");
            assert_eq!(state.before.as_ref().unwrap()["v"], original);
        } else {
            assert!(rejected.is_none());
            assert_eq!(state.before.as_ref().unwrap()["v"], expected);
        }
    }
}
