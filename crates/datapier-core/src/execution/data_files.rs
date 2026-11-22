//! 导入文件的分块校验与不覆盖发布。
//!
//! 指纹只保存摘要，不保存数据；消费者收到的字节来自已校验的同一个缓冲区。
pub(crate) mod csv;
mod json;
mod raw;
mod reader;
pub use raw::{read_raw_record, read_raw_record_with_cancel};
mod writer;
pub use reader::{RowEvent, RowReader};
pub use writer::CsvJsonWriter;

use crate::model::{AppError, Result, transfer::InputFingerprint};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[cfg(windows)]
type Metadata = cap_std::fs::Metadata;
#[cfg(not(windows))]
type Metadata = fs::Metadata;

#[cfg(windows)]
fn file_metadata(file: &File) -> io::Result<Metadata> {
    Metadata::from_file(file)
}
#[cfg(not(windows))]
fn file_metadata(file: &File) -> io::Result<Metadata> {
    file.metadata()
}

#[cfg(windows)]
fn path_metadata(path: &Path) -> io::Result<Metadata> {
    file_metadata(&File::open(path)?)
}
#[cfg(not(windows))]
fn path_metadata(path: &Path) -> io::Result<Metadata> {
    fs::metadata(path)
}

const MAX_BLOCK_BYTES: usize = 16 * 1024 * 1024;

fn changed() -> AppError {
    AppError::conflict("输入文件在准备后发生变化")
}

#[cfg(unix)]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(windows)]
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    use cap_fs_ext::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(not(any(unix, windows)))]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

#[cfg(unix)]
fn file_identity(meta: &fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!("unix:{}:{}", meta.dev(), meta.ino()))
}
#[cfg(windows)]
fn file_identity(meta: &Metadata) -> Option<String> {
    use cap_fs_ext::MetadataExt;
    Some(format!("windows:{}:{}", meta.dev(), meta.ino()))
}
#[cfg(not(any(unix, windows)))]
fn file_identity(_meta: &fs::Metadata) -> Option<String> {
    None
}

fn modified_ns(meta: &Metadata) -> Option<String> {
    let modified = meta.modified().ok()?;
    #[cfg(windows)]
    let modified = modified.into_std();
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|n| n.as_nanos().to_string())
}
/// 扫描输入文件，按固定块建立有明确配额的 SHA-256 索引。
pub fn fingerprint(
    path: &Path,
    block_bytes: usize,
    max_index_bytes: usize,
) -> Result<InputFingerprint> {
    fingerprint_open(path, block_bytes, max_index_bytes).map(|(fingerprint, _)| fingerprint)
}
/// 扫描中检查取消信号，取消时不保留半成品索引。
pub fn fingerprint_with_cancel(
    path: &Path,
    block_bytes: usize,
    max_index_bytes: usize,
    cancel: &CancellationToken,
) -> Result<InputFingerprint> {
    fingerprint_open_with_cancel(path, block_bytes, max_index_bytes, cancel)
        .map(|(fingerprint, _)| fingerprint)
}

/// 建立索引后保留同一个打开句柄，供准备及后续读取使用。
pub fn fingerprint_open(
    path: &Path,
    block_bytes: usize,
    max_index_bytes: usize,
) -> Result<(InputFingerprint, VerifiedReader)> {
    fingerprint_open_inner(path, block_bytes, max_index_bytes, None, None)
}

/// 建立指纹并把取消信号传递给同一打开句柄的后续读取。
pub fn fingerprint_open_with_cancel(
    path: &Path,
    block_bytes: usize,
    max_index_bytes: usize,
    cancel: &CancellationToken,
) -> Result<(InputFingerprint, VerifiedReader)> {
    fingerprint_open_inner(
        path,
        block_bytes,
        max_index_bytes,
        Some(cancel.clone()),
        None,
    )
}

/// 扫描时按完整索引块报告已读取字节和文件大小。
pub fn fingerprint_with_progress(
    path: &Path,
    block_bytes: usize,
    max_index_bytes: usize,
    cancel: &CancellationToken,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<InputFingerprint> {
    fingerprint_open_inner(
        path,
        block_bytes,
        max_index_bytes,
        Some(cancel.clone()),
        Some(progress),
    )
    .map(|(fingerprint, _)| fingerprint)
}
fn fingerprint_open_inner(
    path: &Path,
    block_bytes: usize,
    max_index_bytes: usize,
    cancel: Option<CancellationToken>,
    mut progress: Option<&mut dyn FnMut(u64, u64)>,
) -> Result<(InputFingerprint, VerifiedReader)> {
    if cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
        return Err(AppError::cancelled());
    }
    if block_bytes == 0 || block_bytes > MAX_BLOCK_BYTES {
        return Err(AppError::new("resource_limit", "输入校验块大小超出限制"));
    }
    let path = fs::canonicalize(path)?;
    let mut file = File::open(&path)?;
    let original = file_metadata(&file)?;
    if !original.is_file() {
        return Err(AppError::invalid("输入必须是普通文件"));
    }
    let blocks = original.len().div_ceil(block_bytes as u64);
    // 返回的指纹和打开的 reader 各持有一份索引，配额需计入两份。
    if blocks > (max_index_bytes / 192) as u64 {
        return Err(AppError::new(
            "resource_limit",
            "输入文件校验索引超出内存限制",
        ));
    }
    let mut digests = Vec::with_capacity(blocks as usize);
    let mut full = Sha256::new();
    let mut buffer = vec![0; block_bytes];
    if let Some(report) = progress.as_mut() {
        report(0, original.len());
    }
    let mut bytes = 0u64;
    loop {
        if cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
            return Err(AppError::cancelled());
        }
        let mut used = 0;
        while used < buffer.len() {
            if cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
                return Err(AppError::cancelled());
            }
            let n = file.read(&mut buffer[used..])?;
            if n == 0 {
                break;
            }
            used += n;
        }
        if used == 0 {
            break;
        }
        bytes += used as u64;
        if digests.len() >= blocks as usize {
            return Err(changed());
        }
        full.update(&buffer[..used]);
        digests.push(format!(
            "{:x}",
            base16ct::HexDisplay(&Sha256::digest(&buffer[..used]))
        ));
        if let Some(report) = progress.as_mut() {
            report(bytes, original.len());
        }
    }
    let current = path_metadata(&path)?;
    let after = file_metadata(&file)?;
    if bytes != original.len()
        || after.len() != original.len()
        || !same_file(&original, &current)
        || original.modified().ok() != after.modified().ok()
    {
        return Err(changed());
    }
    let fingerprint = InputFingerprint {
        path,
        bytes,
        sha256: format!("{:x}", base16ct::HexDisplay(&full.finalize())),
        block_bytes,
        block_digests: digests,
        identity: file_identity(&original),
        modified_ns: modified_ns(&original),
    };
    let reader = VerifiedReader {
        file,
        fingerprint: fingerprint.clone(),
        identity: original,
        position: 0,
        block_number: None,
        block: Vec::new(),
        cancel,
    };
    Ok((fingerprint, reader))
}

/// 随机访问读取器；每个块先完整校验，再从该缓冲区交付字节。
pub struct VerifiedReader {
    file: File,
    fingerprint: InputFingerprint,
    identity: Metadata,
    position: u64,
    block_number: Option<u64>,
    block: Vec<u8>,
    cancel: Option<CancellationToken>,
}
impl VerifiedReader {
    pub fn open(fingerprint: &InputFingerprint) -> Result<Self> {
        if fingerprint.block_bytes == 0
            || fingerprint.block_bytes > MAX_BLOCK_BYTES
            || fingerprint.block_digests.len() as u64
                != fingerprint.bytes.div_ceil(fingerprint.block_bytes as u64)
        {
            return Err(AppError::invalid("输入文件校验索引无效"));
        }
        let file = File::open(&fingerprint.path)?;
        let identity = file_metadata(&file)?;
        if identity.len() != fingerprint.bytes {
            return Err(changed());
        }
        if fingerprint.identity.is_some() && fingerprint.identity != file_identity(&identity) {
            return Err(changed());
        }
        if fingerprint.modified_ns.is_some() && fingerprint.modified_ns != modified_ns(&identity) {
            return Err(changed());
        }
        Ok(Self {
            file,
            fingerprint: fingerprint.clone(),
            identity,
            position: 0,
            block_number: None,
            block: Vec::new(),
            cancel: None,
        })
    }
    pub fn set_cancel(&mut self, cancel: CancellationToken) {
        self.cancel = Some(cancel);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }
    fn verify_path(&self) -> io::Result<()> {
        if self.is_cancelled() {
            return Err(io::Error::other(AppError::cancelled()));
        }
        let path = path_metadata(&self.fingerprint.path)?;
        let handle = file_metadata(&self.file)?;
        if !same_file(&self.identity, &path)
            || !same_file(&self.identity, &handle)
            || path.len() != self.fingerprint.bytes
            || handle.len() != self.fingerprint.bytes
            || (self.fingerprint.modified_ns.is_some()
                && self.fingerprint.modified_ns != modified_ns(&handle))
        {
            return Err(io::Error::other(changed()));
        }
        Ok(())
    }
    fn load(&mut self, number: u64) -> io::Result<()> {
        self.verify_path()?;
        let start = number * self.fingerprint.block_bytes as u64;
        let size =
            (self.fingerprint.bytes - start).min(self.fingerprint.block_bytes as u64) as usize;
        self.file.seek(SeekFrom::Start(start))?;
        self.block.resize(size, 0);
        self.file.read_exact(&mut self.block)?;
        let hash = format!("{:x}", base16ct::HexDisplay(&Sha256::digest(&self.block)));
        if self.fingerprint.block_digests.get(number as usize) != Some(&hash) {
            return Err(io::Error::other(changed()));
        }
        self.verify_path()?;
        self.block_number = Some(number);
        Ok(())
    }
}
impl Read for VerifiedReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.verify_path()?;
        if out.is_empty() || self.position >= self.fingerprint.bytes {
            return Ok(0);
        }
        let number = self.position / self.fingerprint.block_bytes as u64;
        if self.block_number != Some(number) {
            self.load(number)?;
        }
        let offset = (self.position % self.fingerprint.block_bytes as u64) as usize;
        let count = out.len().min(self.block.len() - offset);
        out[..count].copy_from_slice(&self.block[offset..offset + count]);
        self.position += count as u64;
        Ok(count)
    }
}
impl Seek for VerifiedReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = match position {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::End(n) => i128::from(self.fingerprint.bytes) + i128::from(n),
            SeekFrom::Current(n) => i128::from(self.position) + i128::from(n),
        };
        if !(0..=i128::from(u64::MAX)).contains(&next) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "无效文件偏移"));
        }
        self.position = next as u64;
        // 重新读取块时重新校验，不能复用先前通过的摘要。
        self.block_number = None;
        Ok(self.position)
    }
}

/// 临时输出文件的可持久化身份；可按需计算完整内容摘要。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OutputSnapshot {
    pub canonical_path: PathBuf,
    pub file_id: Option<String>,
    pub size: u64,
    pub sha256: Option<String>,
}

/// 基于打开的普通文件读取身份和可选摘要；读取期间变化将失败。
pub fn output_snapshot(path: &Path, with_sha256: bool) -> Result<OutputSnapshot> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(AppError::conflict("输出文件已被替换"));
    }
    let canonical_path = fs::canonicalize(path)?;
    let mut file = File::open(path)?;
    let before = file_metadata(&file)?;
    if !before.is_file() || !same_file(&before, &path_metadata(path)?) {
        return Err(AppError::conflict("输出文件已被替换"));
    }
    let sha256 = if with_sha256 {
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        Some(format!("{:x}", base16ct::HexDisplay(&digest.finalize())))
    } else {
        None
    };
    let after = file_metadata(&file)?;
    if !same_file(&before, &after)
        || !same_file(&before, &path_metadata(path)?)
        || before.len() != after.len()
        || modified_ns(&before) != modified_ns(&after)
    {
        return Err(AppError::conflict("输出文件在计算摘要期间发生变化"));
    }
    Ok(OutputSnapshot {
        canonical_path,
        file_id: file_identity(&before),
        size: before.len(),
        sha256,
    })
}

/// 文件发布操作可确认的结果，缺少标记时不得推断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationState {
    NotPublished,
    Published,
    Unknown,
}
pub fn publication_state(error: &AppError) -> PublicationState {
    match error
        .details
        .get("publication")
        .and_then(serde_json::Value::as_str)
    {
        Some("not_published") => PublicationState::NotPublished,
        Some("published") => PublicationState::Published,
        _ => PublicationState::Unknown,
    }
}
pub(super) fn publication_error(mut error: AppError, state: PublicationState) -> AppError {
    if !error.details.is_object() {
        *error.details = serde_json::json!({});
    }
    let value = match state {
        PublicationState::NotPublished => "not_published",
        PublicationState::Published => "published",
        PublicationState::Unknown => "unknown",
    };
    if let Some(details) = error.details.as_object_mut() {
        details.insert("publication".into(), value.into());
    }
    error
}

/// 输出始终写入同目录的显式 partial；只有发布成功才移除该名称。
pub struct AtomicOutput {
    destination: PathBuf,
    partial: PathBuf,
    file: Option<File>,
    written_bytes: u64,
}
impl AtomicOutput {
    pub fn create(destination: &Path) -> Result<Self> {
        let mut partial = destination.as_os_str().to_os_string();
        partial.push(format!(".datapier-partial-{}", Uuid::new_v4()));
        let partial = PathBuf::from(partial);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?;
        Ok(Self {
            destination: destination.to_owned(),
            partial,
            file: Some(file),
            written_bytes: 0,
        })
    }
    pub fn partial_path(&self) -> &Path {
        &self.partial
    }
    /// 返回已交给文件句柄的字节数；不代表输出已经封口或发布。
    pub fn bytes_written(&self) -> u64 {
        self.written_bytes
    }
    pub fn snapshot(&mut self, with_sha256: bool) -> Result<OutputSnapshot> {
        let file = self.file.as_mut().expect("输出文件已关闭");
        file.flush()?;
        let handle = file_metadata(file)?;
        if !same_file(&handle, &path_metadata(&self.partial)?) {
            return Err(AppError::conflict("输出临时文件已被替换"));
        }
        output_snapshot(&self.partial, with_sha256)
    }
    pub fn publish(mut self) -> Result<PathBuf> {
        let mut file = self.file.take().expect("输出文件已关闭");
        let prepare = (|| -> Result<_> {
            file.flush()?;
            file.sync_all()?;
            let handle = file_metadata(&file)?;
            if fs::symlink_metadata(&self.partial)?
                .file_type()
                .is_symlink()
                || !same_file(&handle, &path_metadata(&self.partial)?)
            {
                return Err(AppError::conflict("输出临时文件已被替换"));
            }
            Ok(handle)
        })();
        let handle =
            prepare.map_err(|error| publication_error(error, PublicationState::NotPublished))?;
        fs::hard_link(&self.partial, &self.destination).map_err(|error| {
            let state = if error.kind() == io::ErrorKind::AlreadyExists {
                PublicationState::NotPublished
            } else {
                PublicationState::Unknown
            };
            let error = if error.kind() == io::ErrorKind::AlreadyExists {
                AppError::conflict("导出目标文件已存在")
            } else {
                error.into()
            };
            publication_error(error, state)
        })?;
        let cleanup = (|| -> Result<()> {
            if !same_file(&handle, &path_metadata(&self.destination)?) {
                return Err(AppError::conflict("输出发布时文件身份发生变化"));
            }
            if fs::symlink_metadata(&self.partial)?
                .file_type()
                .is_symlink()
                || !same_file(&handle, &path_metadata(&self.partial)?)
            {
                return Err(AppError::conflict("输出临时文件在发布期间被替换"));
            }
            drop(file);
            fs::remove_file(&self.partial)?;
            Ok(())
        })();
        cleanup.map_err(|error| publication_error(error, PublicationState::Published))?;
        Ok(self.destination.clone())
    }
}
impl Write for AtomicOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.file.as_mut().expect("输出文件已关闭").write(bytes)?;
        self.written_bytes = self.written_bytes.saturating_add(written as u64);
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().expect("输出文件已关闭").flush()
    }
}

/// Datapier JSON 增量 writer；MySQL 使用 v1，Redis 使用 v2。
pub struct JsonWriter {
    output: AtomicOutput,
    redis: bool,
    active_kind: Option<String>,
    first_record: bool,
    in_key: bool,
    first_row: bool,
    column_count: usize,
    sealed: bool,
    failed: bool,
}
impl JsonWriter {
    pub fn mysql(mut output: AtomicOutput, columns: &[crate::model::Column]) -> Result<Self> {
        output.write_all(b"{\"format\":\"datapier\",\"version\":1,\"columns\":")?;
        serde_json::to_writer(&mut output, columns)?;
        output.write_all(b",\"rows\":[")?;
        Ok(Self {
            output,
            redis: false,
            active_kind: None,
            first_record: true,
            in_key: false,
            first_row: true,
            column_count: columns.len(),
            sealed: false,
            failed: false,
        })
    }
    pub fn redis(mut output: AtomicOutput) -> Result<Self> {
        output.write_all(b"{\"format\":\"datapier\",\"version\":2,\"records\":[")?;
        Ok(Self {
            output,
            redis: true,
            active_kind: None,
            first_record: true,
            in_key: false,
            first_row: true,
            column_count: 0,
            sealed: false,
            failed: false,
        })
    }
    pub fn partial_path(&self) -> &Path {
        self.output.partial_path()
    }
    pub fn snapshot(&mut self, with_sha256: bool) -> Result<OutputSnapshot> {
        self.output.snapshot(with_sha256)
    }
    pub fn begin_key(
        &mut self,
        key_b64: &str,
        kind: &str,
        expires_at_ms: &serde_json::Value,
    ) -> Result<()> {
        use base64::Engine as _;
        if !self.redis || self.in_key || self.sealed {
            return Err(AppError::invalid("Redis JSON key 顺序错误"));
        }
        base64::engine::general_purpose::STANDARD
            .decode(key_b64)
            .map_err(|e| AppError::invalid(format!("无效 Base64: {e}")))?;
        if !matches!(kind, "string" | "hash" | "list" | "set" | "zset") {
            return Err(AppError::invalid("不支持的 Redis 类型"));
        }
        if !self.first_record {
            self.output.write_all(b",")?;
        }
        self.first_record = false;
        self.output.write_all(b"{\"key_b64\":")?;
        serde_json::to_writer(&mut self.output, key_b64)?;
        self.output.write_all(b",\"type\":")?;
        serde_json::to_writer(&mut self.output, kind)?;
        self.output.write_all(b",\"expires_at_ms\":")?;
        serde_json::to_writer(&mut self.output, expires_at_ms)?;
        self.output.write_all(b",\"rows\":[")?;
        self.in_key = true;
        self.active_kind = Some(kind.to_owned());
        self.first_row = true;
        Ok(())
    }
    pub fn write_row(&mut self, values: &[crate::model::Value]) -> Result<()> {
        if self.sealed {
            return Err(AppError::invalid("JSON 输出已结束"));
        }
        if self.redis && !self.in_key {
            return Err(AppError::invalid("Redis JSON 元素缺少 key"));
        }
        if !self.redis && values.len() != self.column_count {
            return Err(AppError::invalid("JSON 行列数不匹配"));
        }
        if self.active_kind.as_deref() == Some("hash") {
            match values {
                [
                    crate::model::Value::Bytes(_),
                    crate::model::Value::Bytes(_),
                    crate::model::Value::Integer(expiry),
                ] => reader::validate_field_expiry(expiry)?,
                _ => return Err(AppError::invalid("Hash 行必须包含字段绝对期限")),
            }
        }
        if crate::execution::migration::encoded_size(&values)?
            > crate::model::transfer::MAX_VALUE_BYTES
        {
            self.failed = true;
            return Err(AppError::new("resource_limit", "JSON 单条记录超出内存限制"));
        }
        for value in values {
            reader::validate_value(value)?;
        }
        let first = if self.redis {
            &mut self.first_row
        } else {
            &mut self.first_record
        };
        if !*first {
            self.output.write_all(b",")?;
        }
        *first = false;
        serde_json::to_writer(&mut self.output, values)?;
        Ok(())
    }
    pub fn end_key(&mut self) -> Result<()> {
        if !self.redis || !self.in_key {
            return Err(AppError::invalid("Redis JSON key 顺序错误"));
        }
        self.output.write_all(b"]}")?;
        self.in_key = false;
        self.active_kind = None;
        Ok(())
    }
    /// 先补齐 JSON 结尾，使发布前快照覆盖最终文件的全部字节。
    pub fn seal(&mut self) -> Result<()> {
        if self.failed {
            return Err(AppError::new(
                "resource_limit",
                "JSON 记录超限，不能发布部分文件",
            ));
        }
        if self.sealed {
            return Ok(());
        }
        if self.in_key {
            return Err(AppError::invalid("Redis JSON key 尚未完成"));
        }
        self.output.write_all(b"]}")?;
        self.sealed = true;
        Ok(())
    }
    pub fn finish(mut self) -> Result<PathBuf> {
        self.seal()
            .map_err(|error| publication_error(error, PublicationState::NotPublished))?;
        self.output.publish()
    }
}

#[cfg(test)]
#[path = "../../tests/unit/data_files.rs"]
mod tests;
