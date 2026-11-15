use super::*;

#[tokio::test]
async fn schema_enforces_digest_uniqueness_and_step_ownership() {
    let mut conn = sqlx::SqliteConnection::connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql("PRAGMA foreign_keys=ON;")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0001_config.sql"))
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0005_config.sql"))
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO executions(id,owner,workspace_id,method,params,state) VALUES('e','o','w','transfer','{}','running')").execute(&mut conn).await.unwrap();
    let digest = "a".repeat(64);
    sqlx::query(
        "INSERT INTO transfer_runs VALUES('r','e','w',?,'{}','running','{}',NULL,1,0,NULL)",
    )
    .bind(&digest)
    .execute(&mut conn)
    .await
    .unwrap();
    sqlx::query("INSERT INTO transfer_steps VALUES('r',0,'{}','data','not_sent',?,'{}')")
        .bind(&digest)
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO transfer_key_ops VALUES('r',?,?,0)")
        .bind(&digest)
        .bind("b".repeat(64))
        .execute(&mut conn)
        .await
        .unwrap();
    assert!(
        sqlx::query("INSERT INTO transfer_key_ops VALUES('r',?,?,0)")
            .bind("c".repeat(64))
            .bind("b".repeat(64))
            .execute(&mut conn)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("INSERT INTO transfer_key_ops VALUES('r',?,?,1)")
            .bind("c".repeat(64))
            .bind("d".repeat(64))
            .execute(&mut conn)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("INSERT INTO transfer_key_ops VALUES('r','not-a-hash',?,0)")
            .bind("d".repeat(64))
            .execute(&mut conn)
            .await
            .is_err()
    );
}

#[test]
fn outcome_rejects_oversized_batch_and_raw_error() {
    let mut outcome = TransferStepOutcome {
        ordinal: 0,
        locator: TransferLocator::default(),
        phase: "data".into(),
        state: TransferStepState::Failed,
        digest: "a".repeat(64),
        affected: 0,
        error_code: None,
        database_code: None,
        item_states: vec![TransferStepState::Failed; 501],
        error_record: None,
    };
    assert!(validate_outcome(&outcome).is_err());
    outcome.item_states.clear();
    outcome.error_code = Some("raw database error with values".into());
    assert!(validate_outcome(&outcome).is_err());
    outcome.error_code = Some("constraint_error".into());
    assert!(validate_outcome(&outcome).is_ok());
}

#[tokio::test]
async fn sql_batch_prior_states_follow_each_offset_and_legacy_locator() {
    let root = std::env::temp_dir().join(format!("datapier-sql-prior-{}", uuid::Uuid::new_v4()));
    let store = crate::Store::open(&root).await.unwrap();
    let task = store
        .create_task(
            uuid::Uuid::new_v4(),
            "sql-prior",
            "transfer.execute",
            serde_json::json!({}),
            true,
        )
        .await
        .unwrap();
    let run = uuid::Uuid::new_v4();
    let retry = uuid::Uuid::new_v4();
    let retry_task = store
        .create_task(
            uuid::Uuid::new_v4(),
            "sql-prior",
            "transfer.execute",
            serde_json::json!({}),
            true,
        )
        .await
        .unwrap();
    let context = TransferRunContext::default();
    let digest = "a".repeat(64);
    store
        .transfer_create_run(run, task.id, "sql-prior", &digest, &context, None)
        .await
        .unwrap();
    store
        .transfer_create_run(
            retry,
            retry_task.id,
            "sql-prior",
            &digest,
            &context,
            Some(run),
        )
        .await
        .unwrap();

    let mut batch = TransferStepOutcome {
        ordinal: 0,
        locator: TransferLocator {
            input: Some(0),
            byte_start: Some(10),
            byte_end: Some(40),
            record_offsets: vec![10, 20, 30, 40],
            ..Default::default()
        },
        phase: "data".into(),
        state: TransferStepState::NotSent,
        digest: digest.clone(),
        affected: 0,
        error_code: None,
        database_code: None,
        item_states: Vec::new(),
        error_record: None,
    };
    store.transfer_put_step(run, &batch).await.unwrap();
    batch.state = TransferStepState::Failed;
    batch.item_states = vec![
        TransferStepState::Applied,
        TransferStepState::Failed,
        TransferStepState::RolledBack,
        TransferStepState::NotSent,
    ];
    store.transfer_put_step(run, &batch).await.unwrap();

    let mut legacy = TransferStepOutcome {
        ordinal: 1,
        locator: TransferLocator {
            input: Some(0),
            byte_start: Some(50),
            ..Default::default()
        },
        phase: "data".into(),
        state: TransferStepState::NotSent,
        digest,
        affected: 0,
        error_code: None,
        database_code: None,
        item_states: Vec::new(),
        error_record: None,
    };
    store.transfer_put_step(run, &legacy).await.unwrap();
    legacy.state = TransferStepState::Failed;
    store.transfer_put_step(run, &legacy).await.unwrap();
    for (offset, expected) in [
        (10, TransferStepState::Applied),
        (20, TransferStepState::Failed),
        (30, TransferStepState::RolledBack),
        (40, TransferStepState::NotSent),
        (50, TransferStepState::Failed),
    ] {
        assert_eq!(
            store
                .transfer_prior_input_state(retry, 0, offset, "data")
                .await
                .unwrap(),
            Some(expected)
        );
    }
    assert_eq!(
        store
            .transfer_prior_input_state(retry, 0, 25, "data")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .transfer_prior_input_state(retry, 1, 20, "data")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .transfer_prior_input_state(retry, 0, 20, "schema")
            .await
            .unwrap(),
        None
    );
    store.pool.close().await;
    drop(store);
    tokio::fs::remove_dir_all(root).await.unwrap();
}
