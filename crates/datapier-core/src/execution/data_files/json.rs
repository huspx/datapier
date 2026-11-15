//! 基于可校验 seek 的 Datapier JSON 扫描器；大数组只记录位置，不写临时副本。
use super::VerifiedReader;
use crate::model::{AppError, Result, transfer::MAX_VALUE_BYTES};
use serde::de::DeserializeOwned;
use std::io::{BufRead, BufReader, Seek, SeekFrom};

pub struct JsonScan {
    input: BufReader<VerifiedReader>,
    pos: u64,
    capture: Option<Vec<u8>>,
    value_buffer: Vec<u8>,
    string_buffer: Vec<u8>,
}
impl JsonScan {
    pub fn new(reader: VerifiedReader) -> Self {
        Self {
            input: BufReader::new(reader),
            pos: 0,
            capture: None,
            value_buffer: Vec::new(),
            string_buffer: Vec::new(),
        }
    }
    pub fn verify(&self) -> Result<()> {
        self.input.get_ref().verify_path()?;
        Ok(())
    }
    pub fn is_cancelled(&self) -> bool {
        self.input.get_ref().is_cancelled()
    }
    pub fn into_reader(self) -> VerifiedReader {
        self.input.into_inner()
    }
    pub fn position(&self) -> u64 {
        self.pos
    }
    pub fn seek(&mut self, pos: u64) -> Result<()> {
        self.input.seek(SeekFrom::Start(pos))?;
        self.pos = pos;
        Ok(())
    }
    fn peek(&mut self) -> Result<Option<u8>> {
        Ok(self.input.fill_buf()?.first().copied())
    }
    fn byte(&mut self) -> Result<Option<u8>> {
        let byte = self.peek()?;
        if let Some(b) = byte {
            self.input.consume(1);
            self.pos += 1;
            if let Some(data) = &mut self.capture {
                if data.len() >= MAX_VALUE_BYTES {
                    return Err(AppError::new("resource_limit", "JSON 单条记录超出内存限制"));
                }
                data.push(b);
            }
        }
        Ok(byte)
    }
    fn ws(&mut self) -> Result<()> {
        while matches!(self.peek()?, Some(b' ' | b'\r' | b'\n' | b'\t')) {
            self.byte()?;
        }
        Ok(())
    }
    pub fn expect(&mut self, expected: u8) -> Result<()> {
        self.ws()?;
        if self.byte()? != Some(expected) {
            return Err(self.error("JSON 结构错误"));
        }
        Ok(())
    }
    fn error(&self, message: &str) -> AppError {
        AppError::invalid(message).with_details(serde_json::json!({"byte_offset":self.pos}))
    }
    fn string(&mut self) -> Result<()> {
        if self.byte()? != Some(b'"') {
            return Err(self.error("JSON 字符串缺少引号"));
        }
        self.string_buffer.clear();
        self.string_buffer.push(b'"');
        let mut escaped = false;
        let mut unicode_escape = false;
        loop {
            let b = self.byte()?.ok_or_else(|| self.error("JSON 字符串截断"))?;
            if self.string_buffer.len() >= MAX_VALUE_BYTES {
                return Err(AppError::new("resource_limit", "JSON 字符串超出内存限制"));
            }
            self.string_buffer.push(b);
            if escaped {
                unicode_escape |= b == b'u';
                escaped = false;
                continue;
            }
            if b == b'\\' {
                escaped = true;
                continue;
            }
            if b == b'"' {
                serde_json::from_slice::<serde::de::IgnoredAny>(&self.string_buffer)
                    .map_err(|e| self.error(&format!("JSON 字符串无效: {e}")))?;
                std::str::from_utf8(&self.string_buffer)
                    .map_err(|e| self.error(&format!("JSON UTF-8 无效: {e}")))?;
                if unicode_escape {
                    serde_json::from_slice::<String>(&self.string_buffer)
                        .map_err(|e| self.error(&format!("JSON Unicode 转义无效: {e}")))?;
                }
                return Ok(());
            }
        }
    }
    fn value_inner(&mut self, depth: usize) -> Result<()> {
        if depth > 128 {
            return Err(AppError::new("resource_limit", "JSON 嵌套过深"));
        }
        self.ws()?;
        match self.peek()? {
            Some(b'"') => self.string(),
            Some(b'[') => {
                self.byte()?;
                self.ws()?;
                if self.peek()? == Some(b']') {
                    self.byte()?;
                    return Ok(());
                }
                loop {
                    self.value_inner(depth + 1)?;
                    self.ws()?;
                    match self.byte()? {
                        Some(b']') => return Ok(()),
                        Some(b',') => (),
                        _ => return Err(self.error("JSON 数组损坏")),
                    }
                }
            }
            Some(b'{') => {
                self.byte()?;
                self.ws()?;
                if self.peek()? == Some(b'}') {
                    self.byte()?;
                    return Ok(());
                }
                loop {
                    self.ws()?;
                    self.string()?;
                    self.expect(b':')?;
                    self.value_inner(depth + 1)?;
                    self.ws()?;
                    match self.byte()? {
                        Some(b'}') => return Ok(()),
                        Some(b',') => (),
                        _ => return Err(self.error("JSON 对象损坏")),
                    }
                }
            }
            Some(_) => {
                let mut scalar = [0u8; 1024];
                let mut length = 0;
                while let Some(c) = self.peek()? {
                    if matches!(c, b',' | b']' | b'}' | b' ' | b'\r' | b'\n' | b'\t') {
                        break;
                    }
                    if length == scalar.len() {
                        return Err(AppError::new("resource_limit", "JSON 标量超出限制"));
                    }
                    scalar[length] = c;
                    length += 1;
                    self.byte()?;
                }
                if length == 0
                    || serde_json::from_slice::<serde_json::Value>(&scalar[..length]).is_err()
                {
                    return Err(self.error("JSON 标量损坏"));
                }
                Ok(())
            }
            None => Err(self.error("JSON 提前结束")),
        }
    }
    pub fn skip(&mut self) -> Result<()> {
        self.value_inner(0)
    }
    pub fn value<T: DeserializeOwned>(&mut self) -> Result<T> {
        self.ws()?;
        self.capture = Some(std::mem::take(&mut self.value_buffer));
        let result = self.value_inner(0);
        let mut bytes = self.capture.take().unwrap_or_default();
        result?;
        let parsed =
            serde_json::from_slice(&bytes).map_err(|e| self.error(&format!("JSON 值无效: {e}")));
        bytes.clear();
        self.value_buffer = bytes;
        parsed
    }
    /// 仅完整消费记录之后的类型反序列化错误才允许继续。
    pub fn row<T: DeserializeOwned>(&mut self, record: u64, offset: u64) -> Result<T> {
        self.ws()?;
        self.capture = Some(std::mem::take(&mut self.value_buffer));
        let result = self.value_inner(0);
        let mut bytes = self.capture.take().unwrap_or_default();
        result?;
        let parsed = serde_json::from_slice(&bytes).map_err(|e| AppError::new("conversion", format!("JSON 行类型无效: {e}"))
            .with_details(serde_json::json!({"record":record,"byte_offset":offset,"column":null,"recoverable":true})));
        bytes.clear();
        self.value_buffer = bytes;
        parsed
    }
    pub fn object_next(&mut self, first: &mut bool) -> Result<Option<String>> {
        self.ws()?;
        if self.peek()? == Some(b'}') {
            self.byte()?;
            return Ok(None);
        }
        if !*first {
            self.expect(b',')?;
        } else {
            *first = false;
        }
        let key = self.value()?;
        self.expect(b':')?;
        Ok(Some(key))
    }
    pub fn array_next(&mut self, first: &mut bool) -> Result<bool> {
        self.ws()?;
        if self.peek()? == Some(b']') {
            self.byte()?;
            return Ok(false);
        }
        if !*first {
            self.expect(b',')?;
        } else {
            *first = false;
        }
        Ok(true)
    }
    pub fn finish(&mut self) -> Result<()> {
        self.ws()?;
        if self.peek()?.is_some() {
            Err(self.error("JSON 文件有多余内容"))
        } else {
            Ok(())
        }
    }
    pub fn value_start(&mut self) -> Result<u64> {
        self.ws()?;
        Ok(self.pos)
    }
}
