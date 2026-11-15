use datapier_core::{
    model::project::{ProjectConnection, ProjectQuery, ProjectRecoveryAction, ProjectStageState},
    project,
};
use std::{collections::BTreeMap, fs};
use uuid::Uuid;

#[test]
fn project_save_archive_import_and_conflict_recovery() {
    let root = std::env::temp_dir().join(format!("datapier-project-test-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let staging = root.join("staging");
    fs::create_dir(&staging).unwrap();
    let source = root.join("source");
    let opened = project::create(&source, "Example".into()).unwrap();
    let mut manifest = opened.manifest.clone();
    manifest.connections.push(ProjectConnection {
        alias: "dev".into(),
        provider: "mysql".into(),
        source_kind: "direct".into(),
        database: None,
        description: String::new(),
    });
    manifest.queries.push(ProjectQuery {
        id: Uuid::new_v4(),
        name: "Inspect".into(),
        path: "queries/inspect.sql".into(),
        language: "sql".into(),
        connection_alias: "dev".into(),
        database: None,
    });
    let files = BTreeMap::from([("queries/inspect.sql".into(), "select 1;\n".into())]);
    let preview = project::preview(&opened, manifest, files).unwrap();
    let stage = project::prepare_save(&preview, &preview.digest, &staging).unwrap();
    project::commit(&staging, stage.identity.id).unwrap();
    let saved = project::open(&source).unwrap();
    assert_eq!(saved.files["queries/inspect.sql"], "select 1;\n");
    let archive = root.join("example.datapier.zip");
    project::export_archive(&saved, &archive).unwrap();
    let imported = project::prepare_import(&archive, &root.join("imported"), &staging).unwrap();
    let restored = project::commit_import(&staging, imported.id).unwrap();
    assert_eq!(restored.manifest.id, saved.manifest.id);
    assert_eq!(restored.files, saved.files);
    assert!(project::export_archive(&saved, &archive).is_err());

    assert!(project::commit_import(&staging, imported.id).is_err());
    assert_eq!(
        project::open(&root.join("imported")).unwrap().files,
        saved.files
    );
    let mut files = saved.files.clone();
    files.insert("queries/inspect.sql".into(), "select 2;\n".into());
    let preview = project::preview(&saved, saved.manifest.clone(), files).unwrap();
    let stage = project::prepare_save(&preview, &preview.digest, &staging).unwrap();
    fs::write(source.join("queries/inspect.sql"), "select 3;\n").unwrap();
    assert!(
        project::recover(&staging, stage.identity.id, ProjectRecoveryAction::Continue).is_err()
    );
    assert_eq!(
        project::read_stage(&staging, stage.identity.id)
            .unwrap()
            .state,
        ProjectStageState::RecoveryConflict
    );
    assert_eq!(
        fs::read_to_string(source.join("queries/inspect.sql")).unwrap(),
        "select 3;\n"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_rejects_unsafe_and_colliding_paths() {
    let root = std::env::temp_dir().join(format!("datapier-project-test-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let opened = project::create(&root.join("source"), "Example".into()).unwrap();
    let mut manifest = opened.manifest.clone();
    manifest.connections.push(ProjectConnection {
        alias: "dev".into(),
        provider: "mysql".into(),
        source_kind: "direct".into(),
        database: None,
        description: String::new(),
    });
    manifest.queries.push(ProjectQuery {
        id: Uuid::new_v4(),
        name: "Bad".into(),
        path: "../escape.sql".into(),
        language: "sql".into(),
        connection_alias: "dev".into(),
        database: None,
    });
    let files = BTreeMap::from([("../escape.sql".into(), "select 1".into())]);
    assert!(project::preview(&opened, manifest, files).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn normalized_names_are_rejected_before_writing_and_external_deletion_is_visible() {
    let root = std::env::temp_dir().join(format!("datapier-project-unicode-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let staging = root.join("staging");
    fs::create_dir(&staging).unwrap();
    let opened = project::create(&root.join("project"), "Unicode".into()).unwrap();
    let mut manifest = opened.manifest.clone();
    manifest.connections.push(ProjectConnection {
        alias: "dev".into(),
        provider: "mysql".into(),
        source_kind: "direct".into(),
        database: None,
        description: String::new(),
    });
    for path in ["queries/é.sql", "queries/e\u{301}.sql"] {
        manifest.queries.push(ProjectQuery {
            id: Uuid::new_v4(),
            name: path.into(),
            path: path.into(),
            language: "sql".into(),
            connection_alias: "dev".into(),
            database: None,
        });
    }
    let files = BTreeMap::from([
        ("queries/é.sql".into(), "SELECT 1".into()),
        ("queries/e\u{301}.sql".into(), "SELECT 2".into()),
    ]);
    assert!(project::preview(&opened, manifest.clone(), files).is_err());
    manifest.queries.pop();
    let files = BTreeMap::from([("queries/é.sql".into(), "SELECT 1".into())]);
    let preview = project::preview(&opened, manifest, files).unwrap();
    let stage = project::prepare_save(&preview, &preview.digest, &staging).unwrap();
    project::commit(&staging, stage.identity.id).unwrap();
    let saved = project::open(&root.join("project")).unwrap();
    fs::remove_file(root.join("project/queries/é.sql")).unwrap();
    let external = project::open_external(&root.join("project"), &saved).unwrap();
    assert!(!external.files.contains_key("queries/é.sql"));
    assert!(!external.digests.contains_key("queries/é.sql"));
    let restored = project::restore_baseline(
        &root.join("project"),
        saved.manifest.clone(),
        saved.digests.clone(),
    )
    .unwrap();
    assert_eq!(restored.digests, saved.digests);
    assert!(!restored.files.contains_key("queries/é.sql"));
    let observed = project::open_external(&root.join("project"), &restored).unwrap();
    assert!(!observed.digests.contains_key("queries/é.sql"));
    assert_eq!(saved.files["queries/é.sql"], "SELECT 1");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn interrupted_preparing_stage_recovers_from_verified_log() {
    let root = std::env::temp_dir().join(format!("datapier-project-stage-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let staging = root.join("staging");
    fs::create_dir(&staging).unwrap();
    let opened = project::create(&root.join("project"), "Stage".into()).unwrap();
    let mut manifest = opened.manifest.clone();
    manifest.name = "Changed".into();
    let preview = project::preview(&opened, manifest, BTreeMap::new()).unwrap();
    let mut stage = project::prepare_save(&preview, &preview.digest, &staging).unwrap();
    stage.state = ProjectStageState::Preparing;
    fs::remove_file(std::path::Path::new(&stage.identity.stage_dir).join("0.new")).unwrap();
    fs::write(
        std::path::Path::new(&stage.identity.stage_dir).join("stage.json"),
        serde_json::to_vec(&stage).unwrap(),
    )
    .unwrap();
    let result =
        project::recover(&staging, stage.identity.id, ProjectRecoveryAction::Continue).unwrap();
    assert_eq!(result.state, ProjectStageState::Published);
    assert_eq!(
        project::open(&root.join("project")).unwrap().manifest.name,
        "Changed"
    );
    project::cleanup_stage(&staging, stage.identity.id).unwrap();
    assert_eq!(
        project::open(&root.join("project")).unwrap().manifest.name,
        "Changed"
    );
    fs::remove_dir_all(root).unwrap();
}
