//! CSV 的严格逐字符解码与记录边界解析。
use super::VerifiedReader;
use crate::model::{
    AppError, Result,
    transfer::{CsvOptions, MAX_VALUE_BYTES},
};
use encoding_rs::{Decoder, DecoderResult, Encoding, GB18030, GBK, UTF_8, UTF_16BE, UTF_16LE};
use std::io::{BufReader, Read, Seek, SeekFrom};

pub struct CsvRecord {
    pub fields: Vec<String>,
    pub record: u64,
    pub byte_offset: u64,
    pub byte_end: u64,
    pub line: u64,
}

struct Characters {
    input: BufReader<VerifiedReader>,
    decoder: Decoder,
    offset: u64,
    start: u64,
    pending: Option<(char, u64)>,
    ended: bool,
}
impl Characters {
    fn open(reader: VerifiedReader, name: &str) -> Result<Self> {
        let mut input = BufReader::new(reader);
        let mut bom = [0; 3];
        let mut count = 0;
        while count < bom.len() {
            let n = input.read(&mut bom[count..])?;
            if n == 0 {
                break;
            }
            count += n;
        }
        let specified = match name.to_ascii_lowercase().as_str() {
            "utf-8" | "utf-8-bom" => UTF_8,
            "utf-16le" => UTF_16LE,
            "utf-16be" => UTF_16BE,
            "gbk" => GBK,
            "gb18030" => GB18030,
            _ => return Err(AppError::invalid("不支持的 CSV 编码")),
        };
        let (encoding, offset): (&'static Encoding, u64) =
            if count >= 3 && bom == [0xef, 0xbb, 0xbf] {
                (UTF_8, 3)
            } else if count >= 2 && bom[..2] == [0xff, 0xfe] {
                (UTF_16LE, 2)
            } else if count >= 2 && bom[..2] == [0xfe, 0xff] {
                (UTF_16BE, 2)
            } else {
                (specified, 0)
            };
        input.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            input,
            decoder: encoding.new_decoder_without_bom_handling(),
            offset,
            start: offset,
            pending: None,
            ended: false,
        })
    }
    fn next(&mut self) -> Result<Option<(char, u64)>> {
        if let Some(c) = self.pending.take() {
            return Ok(Some(c));
        }
        if self.ended {
            return Ok(None);
        }
        loop {
            let mut byte = [0];
            let n = self.input.read(&mut byte)?;
            let mut output = [0u8; 8];
            let (status, consumed, written) =
                self.decoder
                    .decode_to_utf8_without_replacement(&byte[..n], &mut output, n == 0);
            self.offset += consumed as u64;
            match status {
                DecoderResult::Malformed(_, _) => {
                    return Err(AppError::invalid("CSV 编码错误")
                        .with_details(serde_json::json!({"byte_offset":self.start})));
                }
                DecoderResult::OutputFull => {
                    return Err(AppError::new("resource_limit", "CSV 解码缓冲区不足"));
                }
                DecoderResult::InputEmpty => {}
            }
            if written > 0 {
                let text = std::str::from_utf8(&output[..written])
                    .map_err(|_| AppError::invalid("CSV 解码结果无效"))?;
                let mut chars = text.chars();
                let c = chars
                    .next()
                    .ok_or_else(|| AppError::invalid("CSV 解码结果为空"))?;
                if chars.next().is_some() {
                    return Err(AppError::invalid("CSV 解码器返回多个字符"));
                }
                let start = self.start;
                self.start = self.offset;
                return Ok(Some((c, start)));
            }
            if n == 0 {
                self.ended = true;
                return Ok(None);
            }
        }
    }
    fn unread(&mut self, value: (char, u64)) {
        self.pending = Some(value);
    }
    fn position(&self) -> u64 {
        self.pending.map_or(self.offset, |(_, offset)| offset)
    }
}

/// 记录解析器保留原始字节偏移及物理行号；不接受失去同步的引号。
pub struct CsvReader {
    chars: Characters,
    delimiter: char,
    record: u64,
    line: u64,
    done: bool,
    raw: Vec<u8>,
    parsed: ::csv::StringRecord,
    parser: ::csv::Reader<std::io::Cursor<Vec<u8>>>,
}
impl CsvReader {
    pub fn open(reader: VerifiedReader, options: &CsvOptions) -> Result<Self> {
        options.validate()?;
        let parser = ::csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .delimiter(options.delimiter.as_bytes()[0])
            .from_reader(std::io::Cursor::new(Vec::new()));
        Ok(Self {
            chars: Characters::open(reader, &options.encoding)?,
            delimiter: options.delimiter.as_bytes()[0] as char,
            record: 0,
            line: 1,
            done: false,
            raw: Vec::new(),
            parsed: ::csv::StringRecord::new(),
            parser,
        })
    }
    pub fn position(&self) -> u64 {
        self.chars.position()
    }
    pub fn verify(&self) -> Result<()> {
        self.chars.input.get_ref().verify_path()?;
        Ok(())
    }
    pub fn is_cancelled(&self) -> bool {
        self.chars.input.get_ref().is_cancelled()
    }
    pub fn into_reader(self) -> VerifiedReader {
        self.chars.input.into_inner()
    }
    pub fn next(&mut self) -> Result<Option<CsvRecord>> {
        self.next_inner().map_err(|mut error| {
            if !error.details.is_object() {
                *error.details = serde_json::json!({});
            }
            if let Some(details) = error.details.as_object_mut() {
                details
                    .entry("record")
                    .or_insert_with(|| serde_json::json!(self.record + 1));
                details
                    .entry("line")
                    .or_insert_with(|| serde_json::json!(self.line));
            }
            error
        })
    }
    fn next_inner(&mut self) -> Result<Option<CsvRecord>> {
        if self.done {
            return Ok(None);
        }
        let first = loop {
            let Some(first) = self.chars.next()? else {
                self.done = true;
                return Ok(None);
            };
            if first.0 == '\n' {
                self.line += 1;
                continue;
            }
            if first.0 == '\r' {
                if let Some(next) = self.chars.next()?
                    && next.0 != '\n'
                {
                    self.chars.unread(next);
                }
                self.line += 1;
                continue;
            }
            break first;
        };
        let start = first.1;
        let line = self.line;
        self.chars.unread(first);
        self.raw.clear();
        let mut field_count = 1usize;
        let mut quoted = false;
        let mut closed = false;
        let mut at_start = true;
        let mut quoted_cr = false;
        let mut total = 0usize;
        loop {
            let current = self.chars.next()?;
            let Some((c, offset)) = current else {
                self.done = true;
                if quoted {
                    return Err(AppError::invalid("CSV 引号未闭合").with_details(serde_json::json!({"byte_offset":start,"record":self.record + 1,"line":line})));
                }
                break;
            };
            total = total.saturating_add(c.len_utf8());
            if total > MAX_VALUE_BYTES {
                return Err(AppError::new("resource_limit", "CSV 单条记录超出内存限制"));
            }
            let mut encoded = [0u8; 4];
            self.raw
                .extend_from_slice(c.encode_utf8(&mut encoded).as_bytes());
            if quoted {
                if c == '"' {
                    quoted_cr = false;
                    match self.chars.next()? {
                        Some(('"', _)) => {
                            self.raw.push(b'"');
                            total = total.saturating_add(1);
                        }
                        Some(other) => {
                            quoted = false;
                            closed = true;
                            self.chars.unread(other);
                        }
                        None => {
                            quoted = false;
                            closed = true;
                        }
                    }
                } else {
                    if c == '\r' || (c == '\n' && !quoted_cr) {
                        self.line += 1;
                    }
                    quoted_cr = c == '\r';
                }
                continue;
            }
            if c == self.delimiter {
                if field_count >= 10_000 {
                    return Err(AppError::new("resource_limit", "CSV 列数超出限制"));
                }
                field_count += 1;
                at_start = true;
                closed = false;
                continue;
            }
            if c == '\r' || c == '\n' {
                if c == '\r'
                    && let Some(next) = self.chars.next()?
                {
                    if next.0 != '\n' {
                        self.chars.unread(next);
                    } else {
                        self.raw.push(b'\n');
                    }
                }
                self.line += 1;
                break;
            }
            if closed {
                return Err(AppError::invalid("CSV 闭合引号后存在多余字符").with_details(serde_json::json!({"byte_offset":offset,"record":self.record + 1,"line":self.line})));
            }
            if c == '"' {
                if !at_start {
                    return Err(AppError::invalid("CSV 未转义引号").with_details(serde_json::json!({"byte_offset":offset,"record":self.record + 1,"line":self.line})));
                }
                quoted = true;
            }
            at_start = false;
        }
        // csv 自身宽松处理异常引号；先通过上面的边界状态机，再复用 csv 解析字段。
        std::mem::swap(self.parser.get_mut().get_mut(), &mut self.raw);
        self.parser
            .seek_raw(SeekFrom::Start(0), ::csv::Position::new())
            .map_err(|e| AppError::new("csv", e.to_string()))?;
        let present = self
            .parser
            .read_record(&mut self.parsed)
            .map_err(|e| AppError::new("csv", e.to_string()))?;
        std::mem::swap(self.parser.get_mut().get_mut(), &mut self.raw);
        if !present {
            return Err(AppError::invalid("CSV 记录无法解析"));
        }
        let fields = self.parsed.iter().map(str::to_owned).collect();
        self.record += 1;
        Ok(Some(CsvRecord {
            fields,
            record: self.record,
            byte_offset: start,
            byte_end: self.chars.position(),
            line,
        }))
    }
}
