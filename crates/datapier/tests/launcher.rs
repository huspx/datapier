#![cfg(unix)]

use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

struct Fixture {
    root: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("datapier-launcher-test-{}", uuid::Uuid::new_v4()));
        let data = root.join("data 中文");
        let cli = root.join(".datapier");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&cli).unwrap();
        fs::copy(
            env!("CARGO_BIN_EXE_datapier-launcher"),
            root.join("datapier"),
        )
        .unwrap();
        let binary = root.join("target program");
        let content = b"#!/bin/sh\nprintf '%s\\n' \"$DATAPIER_DATA_DIR\" \"$1\"\nexit 23\n";
        fs::write(&binary, content).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let hash = format!("{:x}", base16ct::HexDisplay(&Sha256::digest(content)));
        fs::write(
            root.join("release-bin-sha256.txt"),
            format!("{hash}  fixture\n"),
        )
        .unwrap();
        fs::write(
            cli.join("active.json"),
            serde_json::to_vec(&serde_json::json!({
                "binary": binary, "binary_hash": hash, "data_dir": data, "plugin_dir": root
            }))
            .unwrap(),
        )
        .unwrap();
        Self { root, data }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(self.root.join("datapier"));
        command.env_remove("DATAPIER_DATA_DIR");
        command
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn launcher_passes_stdio_arguments_and_exit_status() {
    let fixture = Fixture::new();
    let result = fixture.command().arg("mcp 值").output().unwrap();
    assert_eq!(result.status.code(), Some(23));
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        format!(
            "{}\nmcp 值\n",
            fixture.data.canonicalize().unwrap().display()
        )
    );
}

#[test]
fn launcher_rejects_gate_lock_and_wrong_data_directory() {
    let fixture = Fixture::new();
    fs::write(fixture.data.join("update-gate.json"), "{}").unwrap();
    assert!(
        !fixture
            .command()
            .arg("mcp")
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::remove_file(fixture.data.join("update-gate.json")).unwrap();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(fixture.data.join("update-admission.lock"))
        .unwrap();
    FileExt::try_lock_exclusive(&lock).unwrap();
    assert!(
        !fixture
            .command()
            .arg("mcp")
            .output()
            .unwrap()
            .status
            .success()
    );
    FileExt::unlock(&lock).unwrap();
    assert!(
        !fixture
            .command()
            .env("DATAPIER_DATA_DIR", &fixture.root)
            .arg("mcp")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !fixture
            .command()
            .args(["mcp", "--data-dir-b64", "eA=="])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn launcher_rejects_modified_target() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("target program"), "modified").unwrap();
    let result = fixture.command().arg("mcp").output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8(result.stderr).unwrap().contains("摘要"));
}
