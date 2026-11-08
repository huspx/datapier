//! 旧结果缓存以打开的目录句柄为边界；显示路径不参与删除时的路径解析。
use super::*;
use cap_fs_ext::DirExt;
use cap_std::fs::{Dir, Metadata};
use std::ffi::OsString;

struct Entry {
    parent: Arc<Dir>,
    name: OsString,
    relative: PathBuf,
    preview: Json,
}

struct Snapshot {
    preview: Json,
    entries: Vec<Entry>,
}

fn entry_preview(relative: &Path, metadata: &Metadata) -> Json {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.into_std().duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().to_string());
    json!({"path":relative.to_string_lossy(),"bytes":metadata.len(),
        "directory":metadata.is_dir(),"symlink":metadata.is_symlink(),"modified":modified})
}

impl Snapshot {
    fn collect(root: Arc<Dir>) -> Result<Self> {
        let mut paths = Vec::new();
        let mut bytes = 0u64;
        let mut entries = Vec::new();
        for name in [
            "cache.sqlite3",
            "cache.sqlite3-wal",
            "cache.sqlite3-shm",
            "values",
        ] {
            match root.symlink_metadata(name) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            }
            paths.push(name.to_owned());
            let mut stack = vec![(root.clone(), OsString::from(name), PathBuf::from(name))];
            while let Some((parent, name, relative)) = stack.pop() {
                let mut metadata = parent.symlink_metadata(&name)?;
                if metadata.is_dir() && !metadata.is_symlink() {
                    // 每次仅解析一个名字且不跟随目录链接；持有句柄后，父路径替换不再影响子项。
                    let directory = Arc::new(parent.open_dir_nofollow(&name)?);
                    metadata = directory.dir_metadata()?;
                    for child in directory.entries()? {
                        let child = child?;
                        let name = child.file_name();
                        let relative = relative.join(&name);
                        stack.push((directory.clone(), name, relative));
                    }
                } else if metadata.is_file() {
                    bytes = bytes.saturating_add(metadata.len());
                }
                let preview = entry_preview(&relative, &metadata);
                entries.push(Entry {
                    parent,
                    name,
                    relative,
                    preview,
                });
            }
        }
        let mut previews: Vec<_> = entries.iter().map(|entry| entry.preview.clone()).collect();
        previews.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
        Ok(Self {
            preview: json!({"paths":paths,"bytes":bytes,"entries":previews}),
            entries,
        })
    }

    fn clear(self, expected: &Json) -> Result<Json> {
        if &self.preview != expected {
            return Err(AppError::conflict(
                "旧缓存内容已变化，请重新预览并确认清理范围",
            ));
        }
        for entry in self.entries.iter().rev() {
            let metadata = entry.parent.symlink_metadata(&entry.name)?;
            let directory = metadata.is_dir() && !metadata.is_symlink();
            // 删除子项会改变目录 mtime；目录只删除空目录，类型变化必须重新确认。
            if directory
                != (entry.preview["directory"] == true && entry.preview["symlink"] == false)
                || (!directory && entry_preview(&entry.relative, &metadata) != entry.preview)
            {
                return Err(AppError::conflict(
                    "旧缓存文件已变化，清理未完成，请重新预览",
                ));
            }
            if directory {
                entry.parent.remove_dir(&entry.name)?;
            } else {
                entry.parent.remove_file_or_symlink(&entry.name)?;
            }
        }
        Ok(self.preview)
    }
}

impl Store {
    /// 预览旧版本结果缓存；目录逐级 no-follow 打开，不读取文件正文。
    pub async fn retired_cache_preview(&self) -> Result<Json> {
        let root = self.retired_cache_root.clone();
        tokio::task::spawn_blocking(move || {
            Snapshot::collect(root).map(|snapshot| snapshot.preview)
        })
        .await
        .map_err(std::io::Error::other)?
    }

    /// 删除用户确认的集合；检查与删除均使用已打开的父目录及单个子项名。
    pub async fn clear_retired_cache(&self, expected_preview: &Json) -> Result<Json> {
        let root = self.retired_cache_root.clone();
        let expected = expected_preview.clone();
        tokio::task::spawn_blocking(move || Snapshot::collect(root)?.clear(&expected))
            .await
            .map_err(std::io::Error::other)?
    }
}

#[cfg(test)]
#[path = "../../tests/unit/retired_cache.rs"]
mod tests;
