use super::*;

// 每次只解析一个 JSON 值，避免将完整行数组载入内存。
pub(super) struct JsonReader {
    input: BufReader<File>,
    pending: Option<u8>,
}
impl JsonReader {
    fn new(file: File) -> Self {
        Self {
            input: BufReader::new(file),
            pending: None,
        }
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
        serde_json::from_slice(&data).map_err(Into::into)
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
// 数据字段可能先于元数据出现：暂存其原始 JSON，验证外层后再流式读取。
pub(super) struct JsonSpool {
    path: PathBuf,
}
impl JsonSpool {
    pub(super) fn copy(
        reader: &mut JsonReader,
        root: &Path,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let path = temp_path(&root.join("transfer-json"), "temporary");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        let spool = Self { path };
        let mut writer = BufWriter::new(file);
        reader.copy_value(&mut writer, cancel)?;
        writer.flush()?;
        Ok(spool)
    }
    pub(super) fn reader(&self) -> Result<JsonReader> {
        Ok(JsonReader::new(File::open(&self.path)?))
    }
}
impl Drop for JsonSpool {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(super) struct JsonDocument {
    pub(super) columns: Option<Vec<Column>>,
    pub(super) data: JsonSpool,
}
pub(super) fn json_document(
    file: File,
    root: &Path,
    array: &str,
    cancel: &CancellationToken,
) -> Result<JsonDocument> {
    let mut reader = JsonReader::new(file);
    reader.expect(b'{')?;
    let mut first = true;
    let mut format: Option<String> = None;
    let mut version: Option<u64> = None;
    let mut columns: Option<Vec<Column>> = None;
    let mut data: Option<JsonSpool> = None;
    while let Some(name) = reader.object_next(&mut first)? {
        ensure_not_cancelled(cancel)?;
        match name.as_str() {
            "format" if format.is_none() => format = Some(reader.value()?),
            "version" if version.is_none() => version = Some(reader.value()?),
            "columns" if array == "rows" && columns.is_none() => columns = Some(reader.value()?),
            key if key == array && data.is_none() => {
                data = Some(JsonSpool::copy(&mut reader, root, cancel)?)
            }
            "format" | "version" => return Err(AppError::invalid("JSON 字段重复")),
            "columns" if array == "rows" => return Err(AppError::invalid("JSON 列字段重复")),
            key if key == array => return Err(AppError::invalid("JSON 数据字段重复")),
            _ => reader.copy_value(&mut std::io::sink(), cancel)?,
        }
    }
    reader.end_document()?;
    if format.as_deref() != Some("datapier") || version != Some(1) {
        return Err(AppError::invalid("不是受支持的 Datapier 版本化文件"));
    }
    Ok(JsonDocument {
        columns,
        data: data.ok_or_else(|| AppError::invalid("JSON 缺少数据数组"))?,
    })
}
