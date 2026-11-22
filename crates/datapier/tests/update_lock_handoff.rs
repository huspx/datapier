#![cfg(target_os = "macos")]

use fs2::FileExt;
use std::{
    fs::{self, OpenOptions},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
#[ignore = "仅由父测试启动的继承锁子进程执行"]
fn child_holds_inherited_lock() {
    let Some(dir) = std::env::var_os("DATAPIER_HANDOFF_TEST_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    fs::write(dir.join("ready"), b"ready").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dir.join("done").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn inherited_flock_excludes_an_independent_open_after_parent_closes() {
    let dir = std::env::temp_dir().join(format!("datapier-lock-handoff-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let path = dir.join("engine.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    lock.try_lock_exclusive().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_holds_inherited_lock",
            "--ignored",
            "--nocapture",
        ])
        .env("DATAPIER_HANDOFF_TEST_DIR", &dir)
        .stdin(Stdio::from(lock.try_clone().unwrap()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dir.join("ready").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(lock);
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let error = contender.try_lock_exclusive().unwrap_err();
    assert_eq!(
        error.raw_os_error(),
        fs2::lock_contended_error().raw_os_error()
    );
    fs::write(dir.join("done"), b"done").unwrap();
    assert!(child.wait().unwrap().success());
    contender.try_lock_exclusive().unwrap();
    drop(contender);
    fs::remove_dir_all(dir).unwrap();
}
