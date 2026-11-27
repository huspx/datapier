use super::*;

#[test]
fn release_versions_compare_numeric_components() {
    assert!(version("1.10.0") > version("1.9.9"));
    assert!(version("1.0.0") > version("0.99.99"));
    for invalid in [
        "1.2",
        "1.2.3.4",
        "01.2.3",
        "1.02.3",
        "v1.2.3",
        "1.2.3-rc1",
        "1..3",
    ] {
        assert_eq!(version(invalid), None, "{invalid}");
    }
}

#[test]
fn rejects_downgrades_and_accepts_current_or_newer_versions() {
    let current = env!("CARGO_PKG_VERSION");
    let parts = version(current).unwrap();
    assert!(ensure_target_version(current).is_ok());
    assert!(ensure_target_version(&format!("{}.{}.{}", parts[0] + 1, 0, 0)).is_ok());
    let lower = if parts[2] > 0 {
        format!("{}.{}.{}", parts[0], parts[1], parts[2] - 1)
    } else if parts[1] > 0 {
        format!("{}.{}.0", parts[0], parts[1] - 1)
    } else if parts[0] > 0 {
        format!("{}.0.0", parts[0] - 1)
    } else {
        return;
    };
    let error = ensure_target_version(&lower).unwrap_err();
    assert_eq!(error.code, "update_downgrade");
    assert!(error.message.contains("请安装当前版本或更新版本"));
    assert!(ensure_target_version("invalid").is_err());
}

#[test]
fn status_uses_longer_of_interval_and_rate_limit() {
    let state = State {
        last_attempt: 100,
        rate_limit_until: 200_000,
        ..Default::default()
    };
    let value = output(&state, Path::new("/nonexistent-datapier-test-root"), true);
    assert_eq!(value["next_check_at"], 200_000);
    assert_eq!(value["state"], "unchecked");
}

#[test]
fn interrupted_preinstall_gate_is_recovered_only_with_instance_lock() {
    let root = std::env::temp_dir().join(format!("datapier-update-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let gate = root.join("update-gate.json");
    std::fs::write(
        &gate,
        br#"{"phase":"prepared","id":"6c317109-8c9d-486c-9a6f-3a064bc1a47f"}"#,
    )
    .unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("engine.lock"))
        .unwrap();
    lock.try_lock_exclusive().unwrap();
    assert!(apply::acknowledge(&root, &root).unwrap());
    assert!(gate.exists());
    drop(lock);
    assert!(!apply::acknowledge(&root, &root).unwrap());
    assert!(!gate.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn abandoned_handoff_recovers_without_altering_installed_data() {
    let fixture = std::env::temp_dir().join(format!("datapier-handoff-{}", uuid::Uuid::new_v4()));
    let root = datapier_engine::update_data_root(&fixture.join("data")).unwrap();
    let state = fixture.join("state/plugin-data/datapier");
    let plugin = fixture.join("config/plugins/datapier");
    for directory in [&state, &plugin] {
        fs::create_dir_all(directory).unwrap();
    }
    let state = fs::canonicalize(state).unwrap();
    let plugin = fs::canonicalize(plugin).unwrap();
    let id = uuid::Uuid::new_v4();
    let gate = root.join("update-gate.json");
    let marker = state.join(format!("handoff-{id}.done"));
    let data = root.join("workspace");
    let installed = plugin.join("plugin.toml");
    fs::write(&data, b"original workspace").unwrap();
    fs::write(&installed, b"original plugin").unwrap();
    fs::write(&marker, b"").unwrap();
    fixed_entry::write_record(
        &gate,
        &serde_json::json!({"phase":"handoff","id":id,"plugin_dir":plugin,"state_dir":state}),
    )
    .unwrap();
    let owner = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(root.join("update-owner.lock"))
        .unwrap();
    owner.try_lock_exclusive().unwrap();
    assert_eq!(
        apply::recover(&root, &plugin, &state)
            .await
            .unwrap_err()
            .code,
        "update_busy"
    );
    drop(owner);
    assert!(apply::recover(&root, &plugin, &fixture).await.is_err());
    assert!(gate.exists());
    apply::recover(&root, &plugin, &state).await.unwrap();
    assert!(!gate.exists());
    assert!(!marker.exists());
    assert_eq!(fs::read(&data).unwrap(), b"original workspace");
    assert_eq!(fs::read(&installed).unwrap(), b"original plugin");
    fs::remove_dir_all(fixture).unwrap();
}

#[test]
fn managed_entry_uses_settings_from_the_installed_plugin_config_root() {
    let fixture = std::env::temp_dir().join(format!("datapier-managed-{}", uuid::Uuid::new_v4()));
    let config = fixture.join("config");
    let plugin = config.join("plugins/datapier");
    let state = fixture.join("state/plugin-data/datapier");
    let root = fixture.join("data");
    for directory in [&plugin, &state, &root] {
        fs::create_dir_all(directory).unwrap();
    }
    let root = fs::canonicalize(root).unwrap();
    let entry = fixed_entry::Entry::managed(&state, true).unwrap().unwrap();
    let settings = serde_json::json!({"agents":{"mcp":[{
        "name":"datapier", "command":entry.command(), "transport":"stdio", "args":["mcp"],
        "env":{"DATAPIER_DATA_DIR":root,"USER_FIELD":"preserved"}
    }]}});
    fixed_entry::write_record(&config.join("settings.json"), &settings).unwrap();
    fixed_entry::write_record(
        &state.join("kv.json"),
        &serde_json::json!({"mcp_managed":{
            "host":"local","data_dir":root,"startup":{
                "command":entry.command(),"transport":"stdio","args":["mcp"],"data_dir":root
            }
        }}),
    )
    .unwrap();
    assert_eq!(
        managed_entry::readback(&state, &plugin, &root, Some(&entry)).unwrap(),
        Some(settings)
    );
    drop(entry);
    fs::remove_dir_all(fixture).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_status_does_not_wait_for_download_lock() {
    let root =
        std::env::temp_dir().join(format!("datapier-update-progress-{}", uuid::Uuid::new_v4()));
    let dir = root.join("state");
    fs::create_dir_all(&dir).unwrap();
    let mut state = State {
        state: "downloading".into(),
        ..Default::default()
    };
    save(&dir, &mut state).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("update.lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    let data = root.join("data");
    let plugin = root.join("plugin");
    let mut pending = tokio::spawn(run(
        "status",
        plugin.clone(),
        dir.clone(),
        data.clone(),
        false,
        false,
        None,
    ));
    let response = tokio::time::timeout(std::time::Duration::from_secs(1), &mut pending).await;
    drop(lock);
    let in_time = response.is_ok();
    let value = match response {
        Ok(value) => value.unwrap(),
        Err(_) => pending.await.unwrap(),
    };
    let interrupted = run("status", plugin, dir, data, false, false, None).await;
    fs::remove_dir_all(root).unwrap();
    assert!(in_time, "状态读取不应等待下载完成");
    assert_eq!(value["state"], "downloading");
    assert_eq!(value["gate_blocked"], false);
    assert_eq!(interrupted["state"], "failed");
    assert_eq!(interrupted["ok"], false);
    assert!(interrupted["progress_revision"].as_u64() > value["progress_revision"].as_u64());
}

#[tokio::test]
async fn reports_real_archive_sizes_until_verification() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let dir = std::env::temp_dir().join(format!(
        "datapier-archive-progress-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&dir).unwrap();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut state = State::default();
    for (name, header, phase, total, valid_hash) in [
        (
            "plugin",
            "Content-Length: 6\r\n",
            DownloadPhase::Plugin,
            Some(6),
            true,
        ),
        ("program", "", DownloadPhase::Program, None, false),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/archive", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            stream
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nConnection: close\r\n{header}\r\nabc123")
                        .as_bytes(),
                )
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        });
        let expected = if valid_hash {
            format!("{:x}", base16ct::HexDisplay(&Sha256::digest(b"abc123")))
        } else {
            "0".repeat(64)
        };
        let file = dir.join(name);
        let result =
            download_archive(&client, &url, &file, &expected, phase, &mut state, &dir).await;
        server.await.unwrap();
        assert_eq!(result.is_ok(), valid_hash);
        assert_eq!(fs::read(file).unwrap(), b"abc123");
        let persisted: Value =
            serde_json::from_slice(&fs::read(dir.join("update-state.json")).unwrap()).unwrap();
        assert_eq!(persisted["state"], "verifying");
        assert_eq!(
            persisted["progress"],
            serde_json::json!({"phase":name,"received_bytes":6,"total_bytes":total})
        );
    }
    fs::remove_dir_all(dir).unwrap();
}
