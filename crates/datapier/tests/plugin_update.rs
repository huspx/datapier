use serde_json::Value;
use std::{fs, process::Command};
use uuid::Uuid;

#[test]
fn failed_update_cli_response_retains_platform_capability() {
    let root = std::env::temp_dir().join(format!("datapier-update-cli-{}", Uuid::new_v4()));
    let state_dir = root.join("state");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(state_dir.join("update-state.json"), b"invalid JSON").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_datapier"))
        .arg("--data-dir")
        .arg(root.join("data"))
        .args(["plugin-update", "--action", "status", "--plugin-dir"])
        .arg(root.join("plugin"))
        .arg("--state-dir")
        .arg(&state_dir)
        .output()
        .unwrap();
    fs::remove_dir_all(&root).unwrap();

    assert!(result.status.success());
    let response: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(response["ok"], false);
    assert_eq!(response["state"], "failed");
    assert!(!response["message"].as_str().unwrap().is_empty());
    assert_eq!(
        response["automatic_install_supported"],
        cfg!(target_os = "macos")
    );
}
