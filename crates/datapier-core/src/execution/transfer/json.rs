use super::*;

// 各范围共享同一打开文件，但独立维护偏移；避免重新按路径打开或复制敏感数据。
struct JsonFile {
    file: std::sync::Arc<std::sync::Mutex<File>>,
    position: u64,
    end: u64,
}
impl Read for JsonFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::{Seek, SeekFrom};
        let len = buf
            .len()
            .min(self.end.saturating_sub(self.position) as usize);
        let mut file = self
            .file
            .lock()
            .map_err(|_| std::io::Error::other("JSON 文件锁已损坏"))?;
        file.seek(SeekFrom::Start(self.position))?;
        let read = file.read(&mut buf[..len])?;
        self.position += read as u64;
        Ok(read)
    }
}
impl std::io::Seek for JsonFile {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        use std::io::SeekFrom;
        let next = match position {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(n) => self.position.checked_add_signed(n),
            SeekFrom::End(n) => self.end.checked_add_signed(n),
        }
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "无效 JSON 偏移"))?;
        self.position = next;
        Ok(next)
    }
}

// 每次只解析一个 JSON 值，避免将完整行数组载入内存。
pub(super) struct JsonReader {
    input: BufReader<JsonFile>,
    pending: Option<u8>,
    value_limit: usize,
}
impl JsonReader {
    fn new(file: File, limit: usize) -> Self {
        Self::from_range(
            std::sync::Arc::new(std::sync::Mutex::new(file)),
            0,
            u64::MAX,
            limit,
        )
    }
    fn from_range(
        file: std::sync::Arc<std::sync::Mutex<File>>,
        start: u64,
        end: u64,
        value_limit: usize,
    ) -> Self {
        Self {
            input: BufReader::new(JsonFile {
                file,
                position: start,
                end,
            }),
            pending: None,
            value_limit,
        }
    }
    fn position(&mut self) -> Result<u64> {
        use std::io::Seek;
        Ok(self.input.stream_position()? - u64::from(self.pending.is_some()))
    }
    fn byte(&mut self) -> Result<Option<u8>> {
        if let Some(c) = self.pending.take() {
            return Ok(Some(c));
        }
        let mut b = [0];
        Ok(if self.input.read(&mut b)? == 0 {
            None
        } else {
            Some(b[0])
        })
    }
    fn nonspace(&mut self) -> Result<Option<u8>> {
        loop {
            match self.byte()? {
                Some(b) if b.is_ascii_whitespace() => (),
                c => return Ok(c),
            }
        }
    }
    pub(super) fn expect(&mut self, expected: u8) -> Result<()> {
        if self.nonspace()? != Some(expected) {
            return Err(AppError::invalid(format!(
                "JSON 格式错误，期望 {}",
                expected as char
            )));
        }
        Ok(())
    }
    pub(super) fn value<T: serde::de::DeserializeOwned>(&mut self) -> Result<T> {
        self.value_bounded(self.value_limit).map(|(value, _)| value)
    }
    pub(super) fn value_bounded<T: serde::de::DeserializeOwned>(
        &mut self,
        limit: usize,
    ) -> Result<(T, usize)> {
        let first = self
            .nonspace()?
            .ok_or_else(|| AppError::invalid("JSON 提前结束"))?;
        let mut data = vec![first];
        let mut in_string = first == b'"';
        let mut escape = false;
        let mut depth: i64 = if first == b'{' || first == b'[' { 1 } else { 0 };
        let scalar = depth == 0 && !in_string;
        if in_string || depth > 0 || scalar {
            loop {
                let b = match self.byte()? {
                    Some(b) => b,
                    None if scalar => break,
                    None => return Err(AppError::invalid("JSON 提前结束")),
                };
                if scalar && (b == b',' || b == b']' || b == b'}' || b.is_ascii_whitespace()) {
                    self.pending = Some(b);
                    break;
                }
                if data.len() >= limit {
                    return Err(AppError::new("resource_limit", "JSON 行超过内存预算"));
                }
                data.push(b);
                if in_string {
                    if escape {
                        escape = false;
                    } else if b == b'\\' {
                        escape = true;
                    } else if b == b'"' {
                        in_string = false;
                        if depth == 0 {
                            break;
                        }
                    }
                } else if b == b'"' {
                    in_string = true;
                } else if b == b'{' || b == b'[' {
                    depth += 1;
                } else if b == b'}' || b == b']' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
        }
        let size = data.len();
        Ok((serde_json::from_slice(&data)?, size))
    }
    pub(super) fn array_next(&mut self, first: &mut bool) -> Result<bool> {
        let c = self
            .nonspace()?
            .ok_or_else(|| AppError::invalid("JSON 数组提前结束"))?;
        if c == b']' {
            return Ok(false);
        }
        if !*first {
            if c != b',' {
                return Err(AppError::invalid("JSON 数组缺少逗号"));
            }
        } else {
            self.pending = Some(c);
            *first = false;
        }
        Ok(true)
    }
    pub(super) fn object_next(&mut self, first: &mut bool) -> Result<Option<String>> {
        let c = self
            .nonspace()?
            .ok_or_else(|| AppError::invalid("JSON 对象提前结束"))?;
        if c == b'}' {
            return Ok(None);
        }
        if *first {
            self.pending = Some(c);
            *first = false;
        } else if c != b',' {
            return Err(AppError::invalid("JSON 对象缺少逗号"));
        }
        let name = self.value::<String>()?;
        self.expect(b':')?;
        Ok(Some(name))
    }
    pub(super) fn copy_value(
        &mut self,
        out: &mut impl Write,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let first = self
            .nonspace()?
            .ok_or_else(|| AppError::invalid("JSON 提前结束"))?;
        out.write_all(&[first])?;
        let mut string = first == b'"';
        let mut escaped = false;
        let mut depth: i64 = if first == b'{' || first == b'[' { 1 } else { 0 };
        let scalar = depth == 0 && !string;
        let mut count = 0usize;
        loop {
            if count.is_multiple_of(65536) {
                ensure_not_cancelled(cancel)?;
            }
            count += 1;
            let b = match self.byte()? {
                Some(b) => b,
                None if scalar => break,
                None => return Err(AppError::invalid("JSON 提前结束")),
            };
            if scalar && (b == b',' || b == b']' || b == b'}' || b.is_ascii_whitespace()) {
                self.pending = Some(b);
                break;
            }
            out.write_all(&[b])?;
            if string {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    string = false;
                    if depth == 0 {
                        break;
                    }
                }
            } else if b == b'"' {
                string = true;
            } else if b == b'{' || b == b'[' {
                depth += 1;
            } else if b == b'}' || b == b']' {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
        }
        Ok(())
    }
    pub(super) fn end_document(&mut self) -> Result<()> {
        if self.nonspace()?.is_some() {
            return Err(AppError::invalid("JSON 文件有多余内容"));
        }
        Ok(())
    }
}
// 先定位数据范围再读取元数据，允许字段乱序而无需磁盘暂存。
pub(super) struct JsonRange {
    file: std::sync::Arc<std::sync::Mutex<File>>,
    start: u64,
    end: u64,
}
impl JsonRange {
    pub(super) fn scan(reader: &mut JsonReader, cancel: &CancellationToken) -> Result<Self> {
        let start = reader.position()?;
        reader.copy_value(&mut std::io::sink(), cancel)?;
        let end = reader.position()?;
        Ok(Self {
            file: reader.input.get_ref().file.clone(),
            start,
            end,
        })
    }
    pub(super) fn reader(&self, limit: usize) -> JsonReader {
        JsonReader::from_range(self.file.clone(), self.start, self.end, limit)
    }
}

pub(super) struct JsonDocument {
    pub(super) columns: Option<Vec<Column>>,
    pub(super) data: JsonRange,
}
pub(super) fn json_document(
    file: File,
    array: &str,
    cancel: &CancellationToken,
    limit: usize,
) -> Result<JsonDocument> {
    let mut reader = JsonReader::new(file, limit);
    reader.expect(b'{')?;
    let mut first = true;
    let mut format: Option<String> = None;
    let mut version: Option<u64> = None;
    let mut columns: Option<Vec<Column>> = None;
    let mut data: Option<JsonRange> = None;
    while let Some(name) = reader.object_next(&mut first)? {
        ensure_not_cancelled(cancel)?;
        match name.as_str() {
            "format" if format.is_none() => format = Some(reader.value()?),
            "version" if version.is_none() => version = Some(reader.value()?),
            "columns" if array == "rows" && columns.is_none() => columns = Some(reader.value()?),
            key if key == array && data.is_none() => {
                data = Some(JsonRange::scan(&mut reader, cancel)?)
            }
            "format" | "version" => return Err(AppError::invalid("JSON 字段重复")),
            "columns" if array == "rows" => return Err(AppError::invalid("JSON 列字段重复")),
            key if key == array => return Err(AppError::invalid("JSON 数据字段重复")),
            _ => reader.copy_value(&mut std::io::sink(), cancel)?,
        }
    }
    reader.end_document()?;
    if format.as_deref() != Some("datapier")
        || version != Some(if array == "records" { 2 } else { 1 })
    {
        return Err(AppError::invalid("不是受支持的 Datapier 版本化文件"));
    }
    Ok(JsonDocument {
        columns,
        data: data.ok_or_else(|| AppError::invalid("JSON 缺少数据数组"))?,
    })
}
