use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[test]
fn creates_private_directories_and_rejects_existing_shared_directory_without_chmod() {
    let base = std::env::temp_dir().join(format!("datapier-private-test-{}", uuid::Uuid::new_v4()));
    let private = base.join("new").join("data");
    let canonical = root(&private).unwrap();
    assert_eq!(std::fs::metadata(&canonical).unwrap().mode() & 0o777, 0o700);
    assert_eq!(
        std::fs::metadata(base.join("new")).unwrap().mode() & 0o777,
        0o700
    );
    let shared = base.join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(root(&shared).unwrap_err().code, "permission_denied");
    assert_eq!(std::fs::metadata(&shared).unwrap().mode() & 0o777, 0o755);
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn update_gate_blocks_admission_until_recovery() {
    let path = std::env::temp_dir().join(format!("datapier-gate-test-{}", uuid::Uuid::new_v4()));
    let root = root(&path).unwrap();
    let held = update_admission(&root, AdmissionMode::Exclusive).unwrap();
    assert_eq!(
        update_admission(&root, AdmissionMode::Shared)
            .unwrap_err()
            .code,
        "update_in_progress"
    );
    std::fs::write(root.join("update-gate.json"), "{}").unwrap();
    drop(held);
    assert_eq!(
        update_admission(&root, AdmissionMode::Shared)
            .unwrap_err()
            .code,
        "update_recovery_required"
    );
    std::fs::remove_file(root.join("update-gate.json")).unwrap();
    drop(update_admission(&root, AdmissionMode::Shared).unwrap());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn shared_admissions_coexist_and_exclude_update_until_both_close() {
    let path =
        std::env::temp_dir().join(format!("datapier-admission-test-{}", uuid::Uuid::new_v4()));
    let root = root(&path).unwrap();
    let first = update_admission(&root, AdmissionMode::Shared).unwrap();
    let second = update_admission(&root, AdmissionMode::Shared).unwrap();
    assert_eq!(
        update_admission(&root, AdmissionMode::Exclusive)
            .unwrap_err()
            .code,
        "update_in_progress"
    );
    drop(first);
    assert_eq!(
        update_admission(&root, AdmissionMode::Exclusive)
            .unwrap_err()
            .code,
        "update_in_progress"
    );
    drop(second);
    drop(update_admission(&root, AdmissionMode::Exclusive).unwrap());
    std::fs::remove_dir_all(root).unwrap();
}
