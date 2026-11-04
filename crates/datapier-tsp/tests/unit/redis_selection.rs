use super::*;

fn fixture() -> (App, DeleteContext) {
    let mut app = App::new("test".into());
    app.profile_form(None);
    app.field_set("name", "redis");
    app.field_set("provider", "redis");
    let profile = app.form_profile().unwrap();
    app.workbench.overlay = None;
    app.profile = Some(profile.clone());
    app.tabs[0].profile_id = Some(profile.id);
    let session = json!(Uuid::new_v4());
    app.session = Some(json!({"session_id":session,"profile":profile}));
    let context = DeleteContext {
        tab: app.tabs[0].id,
        generation: app.generation,
        profile: profile.id,
        database: profile.redis_db(),
        session,
        targets: vec![ResourceTarget {
            database: Some(profile.redis_db().to_string()),
            name: "base64:/w==".into(),
            key_b64: Some("/w==".into()),
            kind: "string".into(),
        }],
    };
    (app, context)
}

#[test]
fn confirmation_is_not_a_ready_draft_and_closing_clears_it() {
    let (backend, _, _) = Backend::scripted();
    for escape in [false, true] {
        let (mut app, context) = fixture();
        app.redis_key_delete
            .drafts
            .insert(context.tab, DeleteDraft::Confirm(context.clone()));
        app.workbench.overlay = Some("redis-key-delete".into());
        assert_eq!(app.pending_key_delete_count(), 0);
        assert_eq!(
            app.redis_key_delete_targets()[0].key_b64.as_deref(),
            Some("/w==")
        );
        if escape {
            assert!(app.workbench_key(&tern_sdk::Key::named("escape"), &backend));
        } else {
            assert!(app.workbench_action("overlay-close", &backend));
        }
        assert!(!app.redis_key_delete_tab_pending(context.tab));
        assert!(app.redis_key_delete_targets().is_empty());
    }
    let (mut app, context) = fixture();
    app.redis_key_delete.drafts.insert(
        context.tab,
        DeleteDraft::Opening(context.clone(), Uuid::new_v4()),
    );
    assert_eq!(app.pending_key_delete_count(), 0);
    app.redis_key_delete.drafts.insert(
        context.tab,
        DeleteDraft::Ready(context, json!(Uuid::new_v4())),
    );
    assert_eq!(app.pending_key_delete_count(), 1);
}

#[test]
fn draft_input_freezes_bytes_and_snapshot_never_persists_token() {
    let (mut app, context) = fixture();
    let token = json!(Uuid::new_v4());
    app.redis_key_delete.drafts.insert(
        context.tab,
        DeleteDraft::Ready(context.clone(), token.clone()),
    );
    let input = app.change_input().unwrap();
    assert_eq!(input["token"], token);
    assert_eq!(input["targets"][0]["key_b64"], "/w==");
    let saved = app.redis_key_delete_snapshot(context.tab).unwrap();
    assert!(saved.get("token").is_none());
    assert_eq!(saved["targets"], input["targets"]);
    app.generation += 1;
    assert!(app.change_input().is_err());
}

#[test]
fn late_open_releases_token_after_context_changes() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, context) = fixture();
    let request = Uuid::new_v4();
    let token = json!(Uuid::new_v4());
    app.redis_key_delete
        .opening
        .insert(request, context.clone());
    app.redis_key_delete
        .drafts
        .insert(context.tab, DeleteDraft::Opening(context.clone(), request));
    app.generation += 1;
    assert!(app.redis_selection_response(
        &backend,
        &format!("redis-key-delete.open/{request}"),
        &Ok(json!({"token":token,"operation":"delete_keys","targets":context.targets}))
    ));
    let discard = requests.try_recv().unwrap();
    assert_eq!(discard.method, "change.discard");
    assert_eq!(discard.params["token"], token);
    assert_eq!(app.pending_key_delete_count(), 0);
}

#[test]
fn cancelling_opening_releases_late_token() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, context) = fixture();
    app.redis_key_delete
        .drafts
        .insert(context.tab, DeleteDraft::Confirm(context.clone()));
    app.redis_selection_action(&backend, "redis-key-delete-confirm");
    let open = requests.try_recv().unwrap();
    assert!(app.redis_key_delete_opening());
    assert!(open.params.get("operation").is_none());
    app.redis_selection_action(&backend, "redis-key-delete-cancel");
    assert!(!app.redis_key_delete_tab_pending(context.tab));
    assert!(!app.redis_key_delete_opening());
    let token = json!(Uuid::new_v4());
    app.redis_selection_response(
        &backend,
        &open.tag,
        &Ok(json!({
            "token":token,"operation":"delete_keys","targets":context.targets
        })),
    );
    let discard = requests.try_recv().unwrap();
    assert_eq!(discard.method, "change.discard");
    assert_eq!(discard.params["token"], token);
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 指向隔离 Redis"]
async fn ui_open_request_is_accepted_by_real_redis_provider() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use datapier_core::{ExecutionSink, Store, providers::ProviderSession};
    use tokio_util::sync::CancellationToken;

    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, mut context) = fixture();
    let mut profile = app.profile.clone().unwrap();
    profile.direct_mut().unwrap().port = std::env::var("DATAPIER_TEST_REDIS_PORT")
        .unwrap()
        .parse()
        .unwrap();
    profile.direct_mut().unwrap().redis_db = 15;
    profile.access_mode = datapier_core::model::AccessMode::ReadWrite;
    context.database = 15;
    app.profile = Some(profile.clone());
    let key = format!("datapier-ui-test:{}", Uuid::new_v4());
    context.targets = vec![ResourceTarget {
        database: Some("15".into()),
        name: key.clone(),
        key_b64: Some(STANDARD.encode(key.as_bytes())),
        kind: "string".into(),
    }];
    let root = std::env::temp_dir().join(format!("datapier-ui-test-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let task = store
        .create_task(
            Uuid::new_v4(),
            "ui-test",
            "execution.start",
            json!({}),
            false,
        )
        .await
        .unwrap();
    let sink = ExecutionSink::new(store.clone(), task.id);
    let cancel = CancellationToken::new();
    let mut provider = ProviderSession::connect(profile, None).await.unwrap();
    provider
        .call(
            "execution.run",
            json!({"text":format!("SET {key} value")}),
            &sink,
            &cancel,
        )
        .await
        .unwrap();
    app.redis_key_delete
        .drafts
        .insert(context.tab, DeleteDraft::Confirm(context));
    app.redis_selection_action(&backend, "redis-key-delete-confirm");
    let request = requests.try_recv().unwrap();
    let opened = provider
        .call(&request.method, request.params, &sink, &cancel)
        .await
        .unwrap();
    assert!(opened["token"].as_str().is_some());
    provider
        .call(
            "change.discard",
            json!({"token":opened["token"]}),
            &sink,
            &cancel,
        )
        .await
        .unwrap();
    provider
        .call(
            "execution.run",
            json!({"text":format!("DEL {key}")}),
            &sink,
            &cancel,
        )
        .await
        .unwrap();
    drop(provider);
    drop(store);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn failed_submit_preserves_intent_without_reusable_token() {
    let (mut app, context) = fixture();
    app.redis_key_delete.drafts.insert(
        context.tab,
        DeleteDraft::Ready(context, json!(Uuid::new_v4())),
    );
    app.redis_key_delete_submit_failed();
    assert_eq!(app.tabs[0].pending["operation"], "delete_keys");
    assert!(app.tabs[0].pending.get("token").is_none());
    assert!(!app.tabs[0].pending_live);
    assert_eq!(app.pending_key_delete_count(), 0);
}
