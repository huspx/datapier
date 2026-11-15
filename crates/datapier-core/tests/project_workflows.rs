//! 项目目录、恢复日志和归档的公开文件工作流。
use datapier_core::{
    model::project::{
        ProjectConnection, ProjectQuery, ProjectRecoveryAction, ProjectStageState, ProjectTransfer,
    },
    project,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::PathBuf,
};
use uuid::Uuid;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("datapier-project-workflow-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("staging")).unwrap();
        Self(root)
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn connection(alias: &str) -> ProjectConnection {
    ProjectConnection {
        alias: alias.into(),
        provider: "mysql".into(),
        source_kind: "direct".into(),
        database: None,
        description: String::new(),
    }
}
#[test]
fn archive_rejects_unicode_equivalent_paths_without_publication() {
    let workspace = Workspace::new();
    let opened = project::create(&workspace.0.join("source"), "Unicode".into()).unwrap();
    let mut manifest = opened.manifest.clone();
    manifest.connections.push(connection("dev"));
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
    let archive = workspace.0.join("collision.zip");
    let mut zip = ZipWriter::new(File::create(&archive).unwrap());
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    zip.start_file("datapier.project.json", options).unwrap();
    zip.write_all(&serde_json::to_vec(&manifest).unwrap())
        .unwrap();
    for (path, text) in [
        ("queries/é.sql", "SELECT 1"),
        ("queries/e\u{301}.sql", "SELECT 2"),
    ] {
        zip.start_file(path, options).unwrap();
        zip.write_all(text.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
    let destination = workspace.0.join("imported");
    assert!(project::prepare_import(&archive, &destination, &workspace.0.join("staging")).is_err());
    assert!(!destination.exists());
}

#[test]
fn import_journal_survives_failure_before_payload() {
    let workspace = Workspace::new();
    let source = project::create(&workspace.0.join("source"), "Import".into()).unwrap();
    let archive = workspace.0.join("source.zip");
    project::export_archive(&source, &archive).unwrap();
    let stage_id = Uuid::new_v4();
    let staging = workspace.0.join("staging");
    fs::create_dir(staging.join(stage_id.to_string())).unwrap();
    let result = project::prepare_import_in(
        &archive,
        &workspace.0.join("destination"),
        &staging,
        stage_id,
        true,
        |_| Err(datapier_core::model::AppError::new("storage", "injected")),
    );
    assert!(result.is_err());
    let journal = project::read_import_journal(&staging, stage_id).unwrap();
    assert_eq!(journal.project_id, source.manifest.id);
    assert!(!staging.join(stage_id.to_string()).join("project").exists());
    assert!(project::read_import_stage(&staging, stage_id).is_err());
    assert!(!workspace.0.join("destination").exists());
}

#[test]
fn project_directory_roundtrip_and_interrupted_save_rollback() {
    let workspace = Workspace::new();
    let root = workspace.0.join("project");
    let staging = workspace.0.join("staging");
    let initial = project::create(&root, "共享项目".into()).unwrap();
    let mut manifest = initial.manifest.clone();
    manifest.connections.push(connection("dev"));
    manifest.queries.push(ProjectQuery {
        id: Uuid::new_v4(),
        name: "查询".into(),
        path: "queries/a.sql".into(),
        language: "sql".into(),
        connection_alias: "dev".into(),
        database: None,
    });
    let files = BTreeMap::from([("queries/a.sql".into(), "SELECT '初始';\n".into())]);
    let preview = project::preview(&initial, manifest, files).unwrap();
    let stage = project::prepare_save(&preview, &preview.digest, &staging).unwrap();
    project::commit(&staging, stage.identity.id).unwrap();
    let saved = project::open(&root.join("datapier.project.json")).unwrap();
    assert_eq!(saved.files["queries/a.sql"], "SELECT '初始';\n");

    let archive = workspace.0.join("project.zip");
    project::export_archive(&saved, &archive).unwrap();
    let imported =
        project::prepare_import(&archive, &workspace.0.join("imported"), &staging).unwrap();
    let restored = project::commit_import(&staging, imported.id).unwrap();
    assert_eq!(restored.manifest.id, saved.manifest.id);
    assert_eq!(restored.files, saved.files);
    let mut updated = saved.files.clone();
    updated.insert("queries/a.sql".into(), "SELECT '新版';\n".into());
    let preview = project::preview(&saved, saved.manifest.clone(), updated).unwrap();
    let stage = project::prepare_save(&preview, &preview.digest, &staging).unwrap();
    // 模拟发布内容文件后、登记进度前中断，磁盘上只允许旧版或新版。
    fs::write(root.join("queries/a.sql"), "SELECT '新版';\n").unwrap();
    let rolled_back =
        project::recover(&staging, stage.identity.id, ProjectRecoveryAction::Rollback).unwrap();
    assert_eq!(rolled_back.state, ProjectStageState::Published);
    assert_eq!(project::open(&root).unwrap().files, saved.files);

    fs::write(root.join("queries/a.sql"), "SELECT 'Git checkout';\n").unwrap();
    assert!(project::preview(&saved, saved.manifest.clone(), saved.files.clone()).is_err());
    let external = project::open(&root).unwrap();
    let mut local_draft = saved.files.clone();
    local_draft.insert("queries/a.sql".into(), "SELECT '本机草稿';\n".into());
    assert!(project::preview(&external, saved.manifest.clone(), local_draft.clone()).is_ok());
    fs::write(root.join("queries/a.sql"), "SELECT '外部再次修改';\n").unwrap();
    assert!(project::preview(&external, saved.manifest.clone(), local_draft.clone()).is_err());
    assert_eq!(local_draft["queries/a.sql"], "SELECT '本机草稿';\n");
    fs::write(root.join("queries/a.sql"), "SELECT 'Git checkout';\n").unwrap();
    assert!(project::export_archive(&saved, &workspace.0.join("stale.zip")).is_err());
    assert_eq!(
        fs::read_to_string(root.join("queries/a.sql")).unwrap(),
        "SELECT 'Git checkout';\n"
    );
}

#[test]
fn shared_connection_references_must_exist_and_cannot_embed_profile_ids() {
    let workspace = Workspace::new();
    let opened = project::create(&workspace.0.join("project"), "引用".into()).unwrap();
    let mut manifest = opened.manifest.clone();
    manifest.connections.push(connection("dev"));
    manifest.transfers.push(ProjectTransfer {
        id: Uuid::new_v4(),
        name: "复制".into(),
        path: "transfers/copy.json".into(),
    });
    let spec = json!({"mode":"copy", "provider":"mysql", "source":{"connection_alias":"dev"}, "destination":{"connection_alias":"dev"}});
    let files = |value: &serde_json::Value| {
        BTreeMap::from([("transfers/copy.json".into(), value.to_string())])
    };
    assert!(project::preview(&opened, manifest.clone(), files(&spec)).is_ok());
    let mut missing = spec.clone();
    missing["destination"]["connection_alias"] = json!("other");
    assert!(project::preview(&opened, manifest.clone(), files(&missing)).is_err());
    let mut local = spec.clone();
    local["source"]["profile_id"] = json!(Uuid::new_v4());
    assert!(project::preview(&opened, manifest.clone(), files(&local)).is_err());
    let mut local_database = spec.clone();
    local_database["source"]["database"] = json!("private_database");
    assert!(project::preview(&opened, manifest.clone(), files(&local_database)).is_err());
    let mut local_redis_db = spec.clone();
    local_redis_db["destination"]["redis_db"] = json!(15);
    assert!(project::preview(&opened, manifest.clone(), files(&local_redis_db)).is_err());
    manifest.queries.push(ProjectQuery {
        id: Uuid::new_v4(),
        name: "bad".into(),
        path: "queries/b.sql".into(),
        language: "sql".into(),
        connection_alias: "missing".into(),
        database: None,
    });
    let mut with_query = files(&spec);
    with_query.insert("queries/b.sql".into(), "SELECT 1;".into());
    assert!(project::preview(&opened, manifest, with_query).is_err());
}

#[test]
fn invalid_archives_never_publish_or_escape_destination() {
    let workspace = Workspace::new();
    let staging = workspace.0.join("staging");
    for (case, names) in [
        ("traversal", vec!["../escaped.sql"]),
        ("case_collision", vec!["queries/A.sql", "queries/a.sql"]),
        ("missing_manifest", vec!["queries/a.sql"]),
    ] {
        let archive = workspace.0.join(format!("{case}.zip"));
        let mut zip = ZipWriter::new(File::create(&archive).unwrap());
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for name in names {
            zip.start_file(name, options).unwrap();
            zip.write_all(b"SELECT 1;").unwrap();
        }
        zip.finish().unwrap();
        let destination = workspace.0.join(format!("import-{case}"));
        assert!(project::prepare_import(&archive, &destination, &staging).is_err());
        assert!(!destination.exists());
        assert!(!workspace.0.join("escaped.sql").exists());
    }
    let corrupt = workspace.0.join("corrupt.zip");
    fs::write(&corrupt, b"PK\x03\x04truncated").unwrap();
    assert!(
        project::prepare_import(&corrupt, &workspace.0.join("corrupt-import"), &staging).is_err()
    );
}
