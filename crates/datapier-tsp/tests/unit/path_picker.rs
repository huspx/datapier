use super::*;

#[test]
fn identical_query_names_resolve_within_each_pane() {
    let first = std::env::temp_dir().join("项目 A");
    let second = std::env::temp_dir().join("项目 B");
    for base in [&first, &second] {
        let expected = base.join("sql").join("check.sql");
        assert_eq!(query_path(Some(base), "sql/check.sql").unwrap(), expected);
        assert_eq!(
            query_path(Some(base), expected.to_str().unwrap()).unwrap(),
            expected
        );
    }
    assert_eq!(
        query_path(None, "sql/check.sql").unwrap(),
        Path::new("sql/check.sql")
    );
    #[cfg(windows)]
    for ambiguous in [r"C:check.sql", r"\check.sql"] {
        assert!(query_path(Some(&first), ambiguous).is_err());
    }
}

#[test]
fn listing_preserves_unicode_and_filters_files_without_hiding_folders() {
    let directory = std::env::temp_dir().join(format!("datapier-picker-{}", Uuid::new_v4()));
    std::fs::create_dir_all(directory.join("子目录")).unwrap();
    std::fs::write(directory.join("测试 文件.SQL"), "SELECT 1;").unwrap();
    std::fs::write(directory.join("other.txt"), "text").unwrap();
    let listing = list_directory(Browse {
        directory: None,
        initial: Some(directory.join("new.sql")),
        base: None,
        folders_only: false,
        extensions: "sql".into(),
        filter: String::new(),
    })
    .unwrap();
    assert_eq!(listing.directory, directory);
    assert!(listing.error.is_none());
    assert_eq!(
        listing
            .entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.folder))
            .collect::<Vec<_>>(),
        [("子目录", true), ("测试 文件.SQL", false)]
    );
    assert_eq!(
        std::fs::read_to_string(directory.join("测试 文件.SQL")).unwrap(),
        "SELECT 1;"
    );
    assert!(!directory.join("new.sql").exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn batch_selection_appends_in_order_without_touching_configured_rows() {
    let mut app = App::new("picker-test".into());
    app.create_query_tab();
    app.workbench.transfer.files = 1;
    app.field_set(&app.transfer_key("mode"), "import");
    app.field_set(&app.transfer_key("file-0"), "original.csv");
    app.field_set(&app.transfer_key("file-format-0"), "csv");
    app.field_set(&app.transfer_key("file-mapping-0"), "id:id");
    let base = std::env::temp_dir();
    let selected = [base.join("second.JSON"), base.join("third.csv")];
    app.append_transfer_paths(selected.to_vec()).unwrap();
    assert_eq!(app.workbench.transfer.files, 3);
    assert_eq!(app.field(&app.transfer_key("file-0")), "original.csv");
    assert_eq!(app.field(&app.transfer_key("file-mapping-0")), "id:id");
    for (index, path) in selected.iter().enumerate() {
        assert_eq!(
            app.field(&app.transfer_key(&format!("file-{}", index + 1))),
            path.to_str().unwrap()
        );
    }
    assert_eq!(app.field(&app.transfer_key("file-format-1")), "json");
    assert_eq!(app.field(&app.transfer_key("file-format-2")), "csv");
}
