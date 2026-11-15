//! 失败记录只在用户显式导出报告时从已校验输入重读。
use super::{VerifiedReader, csv::CsvReader, json::JsonScan};
use crate::model::{
    AppError, ProviderKind, Result,
    transfer::{InputFingerprint, MAX_VALUE_BYTES, TransferFormat, TransferInput},
};
use std::io::{Read, Seek, SeekFrom};
use tokio_util::sync::CancellationToken;

/// 按数据记录编号重读原始文件字节，Redis 编号对应完整 key。
/// 返回值包含 CSV 原编码和记录结束符，不包含整份输入或其他记录。
pub fn read_raw_record(
    input: &TransferInput,
    fingerprint: &InputFingerprint,
    provider: ProviderKind,
    record: u64,
) -> Result<Vec<u8>> {
    read_raw_inner(input, fingerprint, provider, record, None)
}

/// 与 read_raw_record 相同，但长时间扫描可由调用方取消。
pub fn read_raw_record_with_cancel(
    input: &TransferInput,
    fingerprint: &InputFingerprint,
    provider: ProviderKind,
    record: u64,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    read_raw_inner(input, fingerprint, provider, record, Some(cancel)).map_err(|error| {
        if cancel.is_cancelled() {
            AppError::cancelled()
        } else {
            error
        }
    })
}

fn read_raw_inner(
    input: &TransferInput,
    fingerprint: &InputFingerprint,
    provider: ProviderKind,
    wanted: u64,
    cancel: Option<&CancellationToken>,
) -> Result<Vec<u8>> {
    if wanted == 0 {
        return Err(AppError::invalid("记录编号必须从 1 开始"));
    }
    if std::fs::canonicalize(&input.path)? != fingerprint.path {
        return Err(AppError::conflict("输入原路径已被替换"));
    }
    let mut reader = VerifiedReader::open(fingerprint)?;
    if let Some(cancel) = cancel {
        reader.set_cancel(cancel.clone());
    }
    let (start, end, mut reader) = match input.format {
        TransferFormat::Csv => csv_span(reader, input, provider, wanted)?,
        TransferFormat::Json => json_span(reader, provider, wanted)?,
        TransferFormat::Sql => return Err(AppError::invalid("SQL 不属于数据文件格式")),
    };
    let length = end
        .checked_sub(start)
        .ok_or_else(|| AppError::invalid("输入记录偏移无效"))?;
    if length > MAX_VALUE_BYTES as u64 {
        return Err(AppError::new("resource_limit", "原始记录超出报告内存限制"));
    }
    if std::fs::canonicalize(&input.path)? != fingerprint.path {
        return Err(AppError::conflict("输入原路径已被替换"));
    }
    reader.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; length as usize];
    reader.read_exact(&mut bytes)?;
    reader.verify_path()?;
    if std::fs::canonicalize(&input.path)? != fingerprint.path {
        return Err(AppError::conflict("输入原路径已被替换"));
    }
    Ok(bytes)
}
fn csv_span(
    reader: VerifiedReader,
    input: &TransferInput,
    provider: ProviderKind,
    wanted: u64,
) -> Result<(u64, u64, VerifiedReader)> {
    let mut csv = CsvReader::open(reader, &input.csv)?;
    if input.csv.header {
        csv.next()?
            .ok_or_else(|| AppError::invalid("CSV 缺少表头"))?;
    }
    if provider == ProviderKind::Mysql {
        for index in 1..=wanted {
            let record = csv
                .next()?
                .ok_or_else(|| AppError::invalid("找不到指定记录"))?;
            if index == wanted {
                return Ok((record.byte_offset, record.byte_end, csv.into_reader()));
            }
        }
    } else {
        let mut index = 0u64;
        let mut range: Option<(u64, u64)> = None;
        while let Some(record) = csv.next()? {
            if record.fields.len() != 11 || record.fields[0] != "2" {
                return Err(AppError::invalid("Redis CSV 记录格式错误"));
            }
            if record.fields[1] == "key" {
                if index == wanted {
                    let (start, end) = range.ok_or_else(|| AppError::invalid("找不到指定记录"))?;
                    return Ok((start, end, csv.into_reader()));
                }
                index += 1;
                if index == wanted {
                    range = Some((record.byte_offset, record.byte_end));
                }
            } else if record.fields[1] == "item" && index == wanted {
                if let Some((_, end)) = &mut range {
                    *end = record.byte_end;
                }
            } else if record.fields[1] != "item" {
                return Err(AppError::invalid("未知 Redis CSV 记录类型"));
            }
        }
        if index == wanted {
            let (start, end) = range.ok_or_else(|| AppError::invalid("找不到指定记录"))?;
            return Ok((start, end, csv.into_reader()));
        }
    }
    Err(AppError::invalid("找不到指定记录"))
}
fn json_span(
    reader: VerifiedReader,
    provider: ProviderKind,
    wanted: u64,
) -> Result<(u64, u64, VerifiedReader)> {
    let mut json = JsonScan::new(reader);
    json.expect(b'{')?;
    let mut first = true;
    let mut format: Option<String> = None;
    let mut version: Option<u64> = None;
    let mut array = None;
    let data_name = if provider == ProviderKind::Mysql {
        "rows"
    } else {
        "records"
    };
    while let Some(key) = json.object_next(&mut first)? {
        match key.as_str() {
            "format" if format.is_none() => format = Some(json.value()?),
            "version" if version.is_none() => version = Some(json.value()?),
            name if name == data_name && array.is_none() => {
                array = Some(json.value_start()?);
                json.skip()?;
            }
            "format" | "version" => return Err(AppError::invalid("JSON 字段重复")),
            name if name == data_name => return Err(AppError::invalid("JSON 数据字段重复")),
            _ => json.skip()?,
        }
    }
    json.finish()?;
    if format.as_deref() != Some("datapier")
        || version
            != Some(if provider == ProviderKind::Redis {
                2
            } else {
                1
            })
    {
        return Err(AppError::invalid("不是受支持的 Datapier 版本化文件"));
    }
    json.seek(array.ok_or_else(|| AppError::invalid("JSON 缺少数据数组"))?)?;
    json.expect(b'[')?;
    let mut first = true;
    for index in 1..=wanted {
        if !json.array_next(&mut first)? {
            return Err(AppError::invalid("找不到指定记录"));
        }
        let start = json.value_start()?;
        json.skip()?;
        if index == wanted {
            let end = json.position();
            return Ok((start, end, json.into_reader()));
        }
    }
    Err(AppError::invalid("找不到指定记录"))
}
