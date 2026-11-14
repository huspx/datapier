use super::*;

#[tokio::test]
async fn archery_batch_is_atomic_and_credentials_belong_to_server() {
    let store = fixture().await;
    let server = ArcheryServer {
        id: Uuid::new_v4(),
        name: "server".into(),
        base_url: "https://example.test/archery/".into(),
        username: "user".into(),
        http_tls: HttpTls::default(),
        totp_mode: TotpMode::Manual,
        revision: 0,
    };
    let saved = store
        .save_archery_server(
            &server,
            0,
            CredentialUpdate::Replace {
                value: "session-secret".into(),
                persist: false,
            },
            CredentialUpdate::Keep,
        )
        .await
        .unwrap();
    assert_eq!(saved.server.revision, 1);
    assert_eq!(saved.credential_mode, CredentialMode::Session);
    let make = |id, instance_id| Profile {
        id,
        name: format!("instance-{instance_id}"),
        revision: 1,
        approval_policy: ApprovalPolicy::Dangerous,
        provider: ProviderKind::Mysql,
        access_mode: AccessMode::ReadOnly,
        connection: ConnectionConfig::Archery(ArcheryConnection {
            server_id: server.id,
            instance_id,
            instance_name: format!("instance-{instance_id}"),
            databases: vec!["db".into()],
            default_database: "db".into(),
        }),
    };
    let first = make(Uuid::new_v4(), 1);
    store
        .apply_archery_profiles(server.id, 1, &[], std::slice::from_ref(&first))
        .await
        .unwrap();
    assert_eq!(
        store.archery_secret(server.id).await.unwrap().as_deref(),
        Some("session-secret")
    );
    assert!(store.secret(first.id).await.unwrap().is_none());
    let second = make(Uuid::new_v4(), 2);
    let conflicting = make(Uuid::new_v4(), 1);
    assert!(
        store
            .apply_archery_profiles(
                server.id,
                1,
                std::slice::from_ref(&first),
                &[second.clone(), conflicting]
            )
            .await
            .is_err()
    );
    assert_eq!(store.profiles().await.unwrap().len(), 1);
    store
        .apply_archery_profiles(
            server.id,
            1,
            std::slice::from_ref(&first),
            &[first.clone(), second.clone()],
        )
        .await
        .unwrap();
    assert_eq!(store.profiles().await.unwrap().len(), 2);
    let mut renamed = first.clone();
    renamed.name = "renamed".into();
    assert!(store.save_profile(&renamed, None, false).await.is_err());
    assert_eq!(
        store
            .profiles()
            .await
            .unwrap()
            .iter()
            .find(|p| p.id == first.id)
            .unwrap()
            .name,
        first.name
    );
    let result = store
        .apply_archery_profiles(
            server.id,
            1,
            &[first.clone(), second.clone()],
            &[renamed.clone(), second.clone()],
        )
        .await
        .unwrap();
    assert_eq!(result["modified"], json!([first.id]));
    assert!(
        store
            .apply_archery_profiles(
                server.id,
                1,
                &[first.clone(), second.clone()],
                std::slice::from_ref(&first)
            )
            .await
            .is_err()
    );
    // 批量修改返回已提交的新 revision，后续 CAS 必须使用该快照。
    let committed: Vec<Profile> = serde_json::from_value(result["profiles"].clone()).unwrap();
    let renamed = committed
        .iter()
        .find(|profile| profile.id == first.id)
        .unwrap()
        .clone();
    assert_eq!(renamed.revision, first.revision + 1);
    assert_eq!(
        serde_json::to_value(&renamed).unwrap(),
        serde_json::to_value(store.profile(first.id).await.unwrap()).unwrap()
    );
    let result = store
        .apply_archery_profiles(
            server.id,
            1,
            &[renamed.clone(), second.clone()],
            std::slice::from_ref(&renamed),
        )
        .await
        .unwrap();
    assert_eq!(result["deleted"], json!([second.id]));
    store
        .apply_archery_profiles(
            server.id,
            1,
            std::slice::from_ref(&renamed),
            &[renamed.clone(), second.clone()],
        )
        .await
        .unwrap();
    assert!(
        store
            .delete_archery_server(server.id, 1, &[first.id])
            .await
            .is_err()
    );
    assert!(
        store
            .apply_archery_profiles(
                server.id,
                0,
                &[first.clone(), second.clone()],
                std::slice::from_ref(&first)
            )
            .await
            .is_err()
    );
    let deleted = store
        .delete_archery_server(server.id, 1, &[first.id, second.id])
        .await
        .unwrap();
    assert_eq!(deleted.len(), 2);
    assert!(store.profiles().await.unwrap().is_empty());
    cleanup(store).await;
}

#[test]
fn credential_update_rejects_unexpected_fields() {
    assert!(
        serde_json::from_value::<CredentialUpdate>(json!({"kind":"keep","persist":true})).is_err()
    );
    assert!(
        serde_json::from_value::<CredentialUpdate>(json!({"kind":"clear","value":"x"})).is_err()
    );
    assert!(
        serde_json::from_value::<CredentialUpdate>(json!({"kind":"replace","value":"x"})).is_err()
    );
}

#[tokio::test]
async fn password_and_totp_updates_are_independent() {
    let store = fixture().await;
    let mut server = ArcheryServer {
        id: Uuid::new_v4(),
        name: "service".into(),
        base_url: "https://example.test/".into(),
        username: "user".into(),
        http_tls: HttpTls::default(),
        revision: 0,
        totp_mode: TotpMode::Automatic,
    };
    let uri = format!(
        "otpauth://totp/test?secret={}",
        totp_rs::Secret::from("12345678901234567890".as_bytes()).to_base32()
    );
    let info = store
        .save_archery_server(
            &server,
            0,
            CredentialUpdate::Replace {
                value: "password".into(),
                persist: false,
            },
            CredentialUpdate::Replace {
                value: uri.clone(),
                persist: false,
            },
        )
        .await
        .unwrap();
    assert!(info.totp_configured);
    assert_eq!(info.totp_credential_mode, CredentialMode::Session);
    let public = serde_json::to_string(&info).unwrap();
    assert!(!public.contains(&uri));
    assert!(!public.contains("password"));
    server = info.server;
    server.totp_mode = TotpMode::Manual;
    let info = store
        .save_archery_server(&server, 1, CredentialUpdate::Keep, CredentialUpdate::Clear)
        .await
        .unwrap();
    assert!(!info.totp_configured);
    assert_eq!(
        store.archery_secret(server.id).await.unwrap().as_deref(),
        Some("password")
    );
    assert!(
        store
            .archery_totp_secret(server.id)
            .await
            .unwrap()
            .is_none()
    );
    cleanup(store).await;
}

#[tokio::test]
async fn reopened_server_preserves_independent_credential_locations() {
    let store = fixture().await;
    for (name, password_saved, totp_saved) in [("password", 1_i64, 0_i64), ("totp", 0, 1)] {
        let server = ArcheryServer {
            id: Uuid::new_v4(),
            name: name.into(),
            base_url: "https://example.test/".into(),
            username: name.into(),
            http_tls: HttpTls::default(),
            revision: 0,
            totp_mode: TotpMode::Manual,
        };
        store
            .save_archery_server(&server, 0, CredentialUpdate::Keep, CredentialUpdate::Keep)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE archery_servers SET credential_saved=?,totp_credential_saved=? WHERE id=?",
        )
        .bind(password_saved)
        .bind(totp_saved)
        .bind(server.id.to_string())
        .execute(&mut *store.pool.acquire().await.unwrap())
        .await
        .unwrap();
    }
    let root = (*store.root).clone();
    store.pool.close().await;
    drop(store);
    let store = Store::open(&root).await.unwrap();
    let servers = store.archery_servers().await.unwrap();
    let password = servers
        .iter()
        .find(|info| info.server.name == "password")
        .unwrap();
    assert_eq!(password.credential_mode, CredentialMode::Keyring);
    assert_eq!(password.totp_credential_mode, CredentialMode::Unknown);
    assert!(!password.totp_configured);
    let totp = servers
        .iter()
        .find(|info| info.server.name == "totp")
        .unwrap();
    assert_eq!(totp.credential_mode, CredentialMode::Unknown);
    assert_eq!(totp.totp_credential_mode, CredentialMode::Keyring);
    assert!(totp.totp_configured);
    cleanup(store).await;
}
