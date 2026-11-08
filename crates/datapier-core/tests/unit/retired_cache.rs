//! 真实目录替换发生在确认快照取得之后，清理不能穿过替换后的父链接。
#![cfg(unix)]
use super::*;
use std::fs;

#[test]
fn replaced_cache_parent_never_deletes_files_behind_the_new_symlink() {
    let base = std::env::temp_dir().join(format!("datapier-cache-parent-{}", Uuid::new_v4()));
    let root = base.join("data");
    let outside = base.join("outside");
    fs::create_dir_all(root.join("values/nested")).unwrap();
    fs::create_dir_all(outside.join("nested")).unwrap();
    fs::write(root.join("values/nested/item"), b"confirmed cache").unwrap();
    fs::write(outside.join("nested/item"), b"unrelated external data").unwrap();
    let directory = Arc::new(Dir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap());
    let snapshot = Snapshot::collect(directory).unwrap();
    let expected = snapshot.preview.clone();
    fs::rename(root.join("values"), base.join("moved-values")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("values")).unwrap();
    let error = snapshot.clear(&expected).unwrap_err();
    assert_eq!(error.code, "conflict");
    assert_eq!(
        fs::read(outside.join("nested/item")).unwrap(),
        b"unrelated external data"
    );
    assert!(
        fs::symlink_metadata(root.join("values"))
            .unwrap()
            .is_symlink()
    );
    assert!(!base.join("moved-values/nested/item").exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn replaced_data_root_keeps_cleanup_bound_to_the_opened_directory() {
    let base = std::env::temp_dir().join(format!("datapier-cache-root-{}", Uuid::new_v4()));
    let root = base.join("data");
    let outside = base.join("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(root.join("cache.sqlite3"), b"confirmed cache").unwrap();
    fs::write(outside.join("cache.sqlite3"), b"unrelated database").unwrap();
    let directory = Arc::new(Dir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap());
    let snapshot = Snapshot::collect(directory).unwrap();
    let expected = snapshot.preview.clone();
    fs::rename(&root, base.join("moved-data")).unwrap();
    std::os::unix::fs::symlink(&outside, &root).unwrap();
    snapshot.clear(&expected).unwrap();
    assert!(!base.join("moved-data/cache.sqlite3").exists());
    assert_eq!(
        fs::read(outside.join("cache.sqlite3")).unwrap(),
        b"unrelated database"
    );
    fs::remove_dir_all(base).unwrap();
}
