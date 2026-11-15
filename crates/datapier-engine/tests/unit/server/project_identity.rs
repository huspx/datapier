use super::*;

#[test]
fn recreated_project_directory_requires_a_new_identity() {
    let root = std::env::temp_dir().join(format!("datapier-project-identity-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let moved = root.with_extension("original");
    let root_text = root.to_str().unwrap();
    let original = binding_root_identity(root_text).unwrap();
    std::fs::rename(&root, &moved).unwrap();
    std::fs::create_dir(&root).unwrap();
    assert_ne!(binding_root_identity(root_text).unwrap(), original);
    let current = stage_entry_identity(&root, &std::fs::symlink_metadata(&root).unwrap()).unwrap();
    assert_eq!(
        binding_root_identity(root_text).unwrap(),
        format!("{root_text}\0{current}")
    );
    std::fs::remove_dir(&root).unwrap();
    std::fs::remove_dir(&moved).unwrap();
}

#[test]
fn replaced_stage_file_does_not_keep_its_identity() {
    let root = std::env::temp_dir().join(format!("datapier-stage-identity-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("value.sql");
    std::fs::write(&path, "SELECT 1").unwrap();
    let original = stage_entry_identity(&path, &std::fs::metadata(&path).unwrap()).unwrap();
    std::fs::rename(&path, root.join("original.sql")).unwrap();
    std::fs::write(&path, "SELECT 1").unwrap();
    assert_ne!(
        stage_entry_identity(&path, &std::fs::metadata(&path).unwrap()).unwrap(),
        original
    );
    std::fs::remove_dir_all(root).unwrap();
}
