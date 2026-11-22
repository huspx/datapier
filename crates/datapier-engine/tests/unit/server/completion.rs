use super::*;

#[test]
fn snapshot_pages_keep_case_distinct_names_and_unknown_completeness() {
    use datapier_core::model::CompletionItem;
    let items = ["A%one", "a%two", "a_other"]
        .into_iter()
        .map(|name| CompletionItem {
            kind: "unknown".into(),
            database: Some("db".into()),
            relation: None,
            name: name.into(),
            data_type: None,
            comment: None,
        })
        .collect();
    let snapshot = CompletionPage {
        items,
        completeness: "unknown".into(),
        next_cursor: None,
        reason: Some("平台可能截断".into()),
    };
    let first = snapshot_page(&snapshot, "a%", None, 1);
    assert_eq!(first.items[0].name, "A%one");
    assert_eq!(first.next_cursor.as_deref(), Some("A%one"));
    let second = snapshot_page(&snapshot, "a%", first.next_cursor.as_deref(), 1);
    assert_eq!(second.items[0].name, "a%two");
    assert_eq!(second.completeness, "unknown");
    assert!(second.next_cursor.is_none());
}

#[test]
fn shared_read_survives_one_subscriber_leaving() {
    let (_, result) = watch::channel(None);
    let flight = Flight {
        cancel: CancellationToken::new(),
        subscribers: AtomicUsize::new(2),
        result,
    };
    flight.release();
    assert!(!flight.cancel.is_cancelled());
    flight.release();
    assert!(flight.cancel.is_cancelled());
}

#[test]
fn late_unsubscribe_cannot_cancel_new_request_and_early_unsubscribe_blocks_old() {
    let mut state = CatalogState::default();
    let owner = Uuid::new_v4();
    let editor = Uuid::new_v4();
    let session = Uuid::new_v4();
    let old = json!("old");
    let new = json!("new");
    let cancel = CancellationToken::new();
    state.subscriptions.insert(
        (owner, editor),
        ActiveSubscription {
            session,
            id: Uuid::new_v4(),
            request_id: new.clone(),
            cancel: cancel.clone(),
        },
    );
    state.unsubscribe(
        owner,
        session,
        editor,
        &old,
        serde_json::to_string(&old).unwrap(),
    );
    assert!(!cancel.is_cancelled());
    assert!(state.admit(owner, editor, &old, 1).is_err());
    assert!(state.admit(owner, editor, &new, 2).is_ok());
    assert!(state.admit(owner, editor, &json!("late"), 1).is_err());
    state.unsubscribe(
        owner,
        session,
        editor,
        &new,
        serde_json::to_string(&new).unwrap(),
    );
    assert!(cancel.is_cancelled());
}

fn page() -> CompletionPage {
    CompletionPage {
        items: Vec::new(),
        completeness: "unknown".into(),
        next_cursor: None,
        reason: None,
    }
}

#[test]
fn invalidation_cancels_reads_and_rejects_old_cursors() {
    let profile = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let token = CancellationToken::new();
    let cursor_id = Uuid::new_v4();
    let mut state = CatalogState::default();
    state.sessions.insert(
        session_id,
        Session {
            profile,
            auth_generation: 0,
            generation: 7,
            cancel: token.clone(),
            permits: Arc::new(Semaphore::new(2)),
        },
    );
    state.cursors.insert(
        cursor_id,
        Cursor {
            session: session_id,
            binding: "scope".into(),
            generation: 7,
            after: "A".into(),
            snapshot_id: None,
            created: Instant::now(),
        },
    );
    state.cache.insert(
        "page".into(),
        Cached {
            profile,
            session: session_id,
            page: Arc::new(page()),
            snapshot_id: Uuid::new_v4(),
            bytes: 1,
            created: Instant::now(),
            used: Instant::now(),
        },
    );
    state.invalidate_profile(profile);
    assert!(token.is_cancelled());
    assert_eq!(state.sessions[&session_id].generation, 8);
    assert!(state.cache.is_empty());
    assert!(state.cursors.is_empty());
    assert!(!state.sessions[&session_id].cancel.is_cancelled());
}

#[test]
fn connection_budget_evicts_oldest_page_for_same_profile() {
    let profile = Uuid::new_v4();
    let other = Uuid::new_v4();
    let now = Instant::now();
    let mut state = CatalogState::default();
    state.cache.insert(
        "old".into(),
        Cached {
            profile,
            session: Uuid::new_v4(),
            page: Arc::new(page()),
            snapshot_id: Uuid::new_v4(),
            bytes: CONNECTION_BYTES - 1,
            created: now,
            used: now,
        },
    );
    state.cache.insert(
        "other".into(),
        Cached {
            profile: other,
            session: Uuid::new_v4(),
            page: Arc::new(page()),
            snapshot_id: Uuid::new_v4(),
            bytes: 1,
            created: now,
            used: now,
        },
    );
    state.trim(profile, 2);
    assert!(!state.cache.contains_key("old"));
    assert!(state.cache.contains_key("other"));
}
