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
