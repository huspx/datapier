//! 手工安装、旧版本回退和中断恢复共用原生更新事务。
use super::*;
use sqlx::{ConnectOptions, Connection, sqlite::SqliteConnectOptions};

pub enum Switch {
    Install {
        version: String,
        package: PathBuf,
        restore_data: Option<PathBuf>,
    },
    Recover,
}

pub async fn switch(plan: Switch, plugin: &Path, state: &Path, root: &Path) -> Result<()> {
    if matches!(&plan, Switch::Install { .. }) {
        ensure_installed_plugin(plugin).await?;
        if !plugin.join("release-bin-sha256.txt").is_file()
            || std::env::var_os("DATAPIER_BINARY").is_some()
        {
            return Err(AppError::invalid("只支持正式安装的 plugin"));
        }
    }
    fs::create_dir_all(state)?;
    let plugin = fs::canonicalize(plugin)?;
    let state = fs::canonicalize(state)?;
    let root = datapier_engine::update_data_root(root)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state.join("update.lock"))?;
    lock.try_lock_exclusive()
        .map_err(|error| AppError::new("update_busy", error.to_string()))?;
    let Switch::Install {
        version: requested,
        package,
        restore_data,
    } = plan
    else {
        apply::recover(&root, &plugin, &state).await?;
        let mut status = State {
            state: "current".into(),
            message: "已恢复中断的更新".into(),
            ..State::default()
        };
        return save(&state, &mut status);
    };
    if version(&requested).is_none() {
        return Err(AppError::invalid("目标版本无效"));
    }
    if root.join("update-gate.json").try_exists()? {
        return Err(AppError::new(
            "update_recovery_required",
            "请先恢复未完成的更新",
        ));
    }
    let target = target().ok_or_else(|| AppError::invalid("不支持的平台"))?;
    let package = fs::canonicalize(package)?;
    let stage = managed_entry::stage_path(&state);
    let restore_data = restore_data.map(fs::canonicalize).transpose()?;
    if let Some(backup) = &restore_data
        && (root.starts_with(backup)
            || backup.starts_with(&root)
            || stage.starts_with(backup)
            || plugin.starts_with(backup)
            || backup.starts_with(&plugin)
            || !backup.join("datapier.sqlite3").is_file())
    {
        return Err(AppError::invalid(
            "备份必须位于独立目录且包含 datapier.sqlite3",
        ));
    }
    managed_entry::create_stage(&stage)?;
    let result = async {
        let plugin_archive = stage.join("plugin.zip");
        fs::copy(
            package.join(format!("datapier-plugin-{requested}.zip")),
            &plugin_archive,
        )?;
        let staged_plugin = extract_plugin(&stage, &requested)?;
        let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
        let program = package.join(format!("datapier-{requested}-{target}.{extension}"));
        prepare_switch_program(&stage, &staged_plugin, &program, &target).await?;
        let next = State {
            plugin_digest: Some(apply::digest(&plugin_archive)?),
            target_version: Some(requested),
            staged_dir: Some(stage.clone()),
            ..State::default()
        };
        apply::run(
            &next,
            &root,
            &plugin,
            &state,
            None,
            None,
            Some(apply::Manual {
                restore_data: restore_data.as_deref(),
            }),
        )
        .await
    }
    .await;
    if result.is_err() && !root.join("update-gate.json").exists() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

pub(super) async fn check_schema(root: &Path, target: &str) -> Result<()> {
    let max_schema = match version(target) {
        Some([0, 1, 0]) => 1,
        Some([0, 1, _]) => 2,
        Some([0, 2, _]) => 3,
        Some([0, 3, _]) => 4,
        Some([0, 4, _]) => 5,
        Some([0, 5, 0..=2]) => 6,
        _ => return Ok(()),
    };
    let database = root.join("datapier.sqlite3");
    if !database.try_exists()? {
        return Ok(());
    }
    let mut connection = SqliteConnectOptions::new()
        .filename(database)
        .read_only(true)
        .connect()
        .await?;
    let schema: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut connection)
        .await?;
    connection.close().await?;
    if schema > max_schema {
        return Err(AppError::new(
            "storage_version",
            format!(
                "目标版本可读配置 schema {max_schema}，当前为 {schema}。请用 --restore-data 指定兼容的升级前一致备份"
            ),
        ));
    }
    Ok(())
}
