//! 用户级 MCP 入口的归属、发布及更新目标。
use datapier_core::{AppError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};

const LAUNCHER: &str = if cfg!(windows) {
    "datapier-launcher.exe"
} else {
    "datapier-launcher"
};

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(super) struct Active {
    pub(super) binary: PathBuf,
    binary_hash: String,
    data_dir: PathBuf,
    plugin_dir: PathBuf,
    launcher_hash: String,
}

pub(super) struct Entry {
    command: PathBuf,
    record: PathBuf,
    _lock: File,
}

fn environment_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn paths() -> Result<(PathBuf, PathBuf)> {
    #[cfg(windows)]
    let (command, directory) = {
        let directory = environment_path("LOCALAPPDATA")
            .ok_or_else(|| AppError::invalid("无法确定 LOCALAPPDATA"))?
            .join("Datapier/bin");
        (directory.join("datapier.exe"), directory)
    };
    #[cfg(unix)]
    let (command, directory) = {
        let home = environment_path("HOME").ok_or_else(|| AppError::invalid("无法确定用户目录"))?;
        let directory = home.join(".local/bin");
        (directory.join("datapier"), directory.join(".datapier"))
    };
    Ok((command, directory.join("active.json")))
}

pub(super) fn regular_digest(path: &Path) -> Result<String> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(AppError::invalid("命令行入口或缓存目标必须是普通文件"));
    }
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hash.update(&buffer[..size]);
    }
    Ok(format!("{:x}", base16ct::HexDisplay(&hash.finalize())))
}

pub(super) fn verify(path: &Path, manifest: &Path, target: &str) -> Result<String> {
    let actual = regular_digest(path)?;
    let manifest = fs::read_to_string(manifest)?;
    let mut lines = manifest.lines().filter_map(|line| {
        let (hash, key) = line.split_once("  ")?;
        (key == target).then_some(hash)
    });
    if lines.next() != Some(actual.as_str()) || lines.next().is_some() {
        return Err(AppError::invalid(format!(
            "程序摘要不匹配：{}",
            path.display()
        )));
    }
    Ok(actual)
}

pub(super) fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path.parent().expect("absolute path"))?.sync_all()?;
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

pub(super) fn copy_synced(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::copy(source, destination)?;
        File::open(destination)?.sync_all()?;
    }
    #[cfg(windows)]
    {
        let mut source = File::open(source)?;
        let mut destination = File::create_new(destination)?;
        io::copy(&mut source, &mut destination)?;
        destination.sync_all()?;
        destination.set_permissions(source.metadata()?.permissions())?;
    }
    Ok(())
}

pub(super) fn write_record(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_file_name(format!(".datapier-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        serde_json::to_writer(&mut file, value)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

impl Entry {
    pub(super) fn lock(create: bool) -> Result<Option<Self>> {
        let (command, record) = paths()?;
        Self::lock_paths(command, record, create)
    }

    fn lock_paths(command: PathBuf, record: PathBuf, create: bool) -> Result<Option<Self>> {
        let directory = record.parent().expect("absolute path");
        if create {
            fs::create_dir_all(directory)?;
        }
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("entry.lock"))
        {
            Ok(file) => file,
            Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        fs2::FileExt::try_lock_exclusive(&file)
            .map_err(|error| AppError::new("update_busy", error.to_string()))?;
        Ok(Some(Self {
            command,
            record,
            _lock: file,
        }))
    }

    pub(super) fn managed(state_dir: &Path, create: bool) -> Result<Option<Self>> {
        let directory = state_dir.join("mcp-entry");
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(AppError::invalid("Tern 受管入口目录已被占用")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let command = directory.join(if cfg!(windows) {
            "datapier.exe"
        } else {
            "datapier"
        });
        #[cfg(unix)]
        let directory = directory.join(".datapier");
        Self::lock_paths(command, directory.join("active.json"), create)
    }

    pub(super) fn command(&self) -> &Path {
        &self.command
    }

    pub(super) fn verify_installed(&self, plugin: &Path, root: &Path, target: &str) -> Result<()> {
        let active = self
            .owned(plugin, root)?
            .ok_or_else(|| AppError::invalid("Tern 受管入口登记已丢失"))?;
        self.check_command(&active)?;
        if verify(
            &active.binary,
            &plugin.join("release-bin-sha256.txt"),
            target,
        )? != active.binary_hash
        {
            return Err(AppError::invalid("Tern 受管入口目标与已安装插件不一致"));
        }
        Ok(())
    }

    fn read(&self) -> Result<Option<Active>> {
        match fs::symlink_metadata(&self.record) {
            Ok(metadata) if metadata.is_file() => {
                Ok(Some(serde_json::from_slice(&fs::read(&self.record)?)?))
            }
            Ok(_) => Err(AppError::invalid("固定入口的登记路径已被占用")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn owned(&self, plugin_dir: &Path, data_dir: &Path) -> Result<Option<Active>> {
        let Some(active) = self.read()? else {
            return Ok(None);
        };
        if active.plugin_dir != plugin_dir || active.data_dir != data_dir {
            return Err(AppError::invalid("固定入口已由另一套 Datapier 安装管理"));
        }
        Ok(Some(active))
    }

    pub(super) fn transition_from(
        &self,
        plugin_dir: &Path,
        data_dir: &Path,
    ) -> Result<Option<Active>> {
        let Some(active) = self
            .read()?
            .filter(|active| active.plugin_dir == plugin_dir)
        else {
            return Ok(None);
        };
        if active.data_dir != data_dir {
            return Err(AppError::invalid("请使用固定入口登记的数据目录更新此安装"));
        }
        Ok(Some(active))
    }

    fn check_command(&self, active: &Active) -> Result<()> {
        if regular_digest(&self.command)? != active.launcher_hash {
            return Err(AppError::invalid(
                "固定入口已被替换，请保留该文件并手工处理",
            ));
        }
        Ok(())
    }

    pub(super) fn next(&self, old: &Active, binary: &Path, binary_hash: String) -> Result<Active> {
        self.check_command(old)?;
        let mut next = old.clone();
        next.binary = fs::canonicalize(binary)?;
        next.binary_hash = binary_hash;
        self.check_target(&next)?;
        Ok(next)
    }

    fn check_target(&self, active: &Active) -> Result<()> {
        if regular_digest(&active.binary)? != active.binary_hash {
            return Err(AppError::invalid("固定入口目标已改变"));
        }
        Ok(())
    }

    pub(super) fn switch(&self, previous: &Active, next: &Active) -> Result<()> {
        let state = self.owned(&previous.plugin_dir, &previous.data_dir)?;
        if state.as_ref() != Some(previous) && state.as_ref() != Some(next) {
            return Err(AppError::invalid("固定入口的登记已改变"));
        }
        self.check_command(previous)?;
        self.check_target(next)?;
        if state.as_ref() != Some(next) {
            write_record(&self.record, next)?;
        }
        Ok(())
    }

    pub(super) fn install(
        &self,
        plugin_dir: PathBuf,
        data_dir: PathBuf,
        target: &str,
    ) -> Result<&Path> {
        if std::env::var_os("DATAPIER_BINARY").is_some() {
            return Err(AppError::invalid("开发安装不发布固定入口"));
        }
        let binary = std::env::current_exe()?.canonicalize()?;
        let binary_hash = verify(&binary, &plugin_dir.join("release-bin-sha256.txt"), target)?;
        if let Some(active) = self.owned(&plugin_dir, &data_dir)? {
            match fs::symlink_metadata(&self.command) {
                Ok(_) => {
                    // 已存在的入口只由受管的更新事务切换目标。
                    self.check_command(&active)?;
                    self.check_target(&active)?;
                    return Ok(&self.command);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.check_target(&active)?;
                    let launcher = active.binary.with_file_name(LAUNCHER);
                    if regular_digest(&launcher)? != active.launcher_hash {
                        return Err(AppError::invalid("固定入口 launcher 摘要不匹配"));
                    }
                    self.publish(&launcher, &active.launcher_hash)?;
                    return Ok(&self.command);
                }
                Err(error) => return Err(error.into()),
            }
        }
        match fs::symlink_metadata(&self.command) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(_) => return Err(AppError::invalid("固定入口路径已有文件或链接")),
        }
        let launcher = binary.with_file_name(LAUNCHER);
        let launcher_hash = verify(
            &launcher,
            &plugin_dir.join("release-launcher-sha256.txt"),
            target,
        )?;
        // 先登记，随后无覆盖地发布；进程中断后可按原记录完成同一次安装。
        write_record(
            &self.record,
            &Active {
                binary,
                binary_hash,
                data_dir,
                plugin_dir,
                launcher_hash: launcher_hash.clone(),
            },
        )?;
        self.publish(&launcher, &launcher_hash)?;
        Ok(&self.command)
    }

    fn publish(&self, launcher: &Path, expected: &str) -> Result<()> {
        let directory = self.command.parent().expect("absolute path");
        fs::create_dir_all(directory)?;
        let temporary = self
            .command
            .with_file_name(format!(".datapier-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            copy_synced(launcher, &temporary)?;
            if regular_digest(&temporary)? != expected {
                return Err(AppError::invalid("复制期间 launcher 摘要发生变化"));
            }
            fs::hard_link(&temporary, &self.command)?;
            sync_parent(&self.command)
        })();
        let _ = fs::remove_file(temporary);
        result
    }

    pub(super) fn remove(&self, plugin_dir: &Path, data_dir: &Path) -> Result<()> {
        if let Some(active) = self.owned(plugin_dir, data_dir)? {
            match fs::symlink_metadata(&self.command) {
                Ok(_) => {
                    self.check_command(&active)?;
                    fs::remove_file(&self.command)?;
                    sync_parent(&self.command)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            fs::remove_file(&self.record)?;
            sync_parent(&self.record)?;
        }
        Ok(())
    }
}
