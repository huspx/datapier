use super::*;
pub(super) fn decode_hex(s: &str, offset: u64, line: u64) -> Result<Vec<u8>> {
    if s.len() > LIMIT * 2 {
        return Err(err(offset, line, "单值超过 16 MiB"));
    }
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(err(offset, line, "非法十六进制字面量"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| err(offset, line, "非法十六进制字面量"))
        })
        .collect()
}

fn decode_hex_number(s: &str, offset: u64, line: u64) -> Result<Vec<u8>> {
    if s.is_empty() {
        return Err(err(offset, line, "空十六进制字面量"));
    }
    if s.len().is_multiple_of(2) {
        decode_hex(s, offset, line)
    } else {
        decode_hex(&format!("0{s}"), offset, line)
    }
}

fn decode_bits(bits: &str, offset: u64, line: u64) -> Result<Value> {
    if bits.is_empty() || bits.len() > LIMIT * 8 || !bits.bytes().all(|b| b == b'0' || b == b'1') {
        return Err(err(offset, line, "非法或超限 bit 字面量"));
    }
    let mut bytes = vec![0u8; bits.len().div_ceil(8)];
    let padding = bytes.len() * 8 - bits.len();
    for (i, b) in bits.bytes().enumerate() {
        if b == b'1' {
            bytes[(i + padding) / 8] |= 1 << (7 - (i + padding) % 8);
        }
    }
    Ok(Value::Bytes(STANDARD.encode(bytes)))
}

fn character_bytes(out: Vec<u8>, charset: Option<&str>, offset: u64, line: u64) -> Result<Value> {
    if out.len() > LIMIT {
        return Err(err(offset, line, "单值超过 16 MiB"));
    }
    if charset.is_some_and(|c| c.eq_ignore_ascii_case("binary")) {
        return Ok(Value::Bytes(STANDARD.encode(out)));
    }
    if charset.is_some_and(|c| c.eq_ignore_ascii_case("ascii")) && !out.is_ascii() {
        return Err(err(offset, line, "ASCII 字面量包含非 ASCII 字节"));
    }
    let encoding = match charset {
        Some(name) => encoding_for(name)?,
        None => UTF_8,
    };
    let text = encoding
        .decode_without_bom_handling_and_without_replacement(&out)
        .ok_or_else(|| err(offset, line, "SQL 字面量字符编码错误"))?;
    Ok(Value::Text(text.into_owned()))
}

fn literal(s: &str, context: &SqlContext, offset: u64, line: u64) -> Result<Value> {
    let s = s.trim();
    if s.len() > LIMIT * 8 + 128 {
        return Err(err(offset, line, "单值超过 16 MiB"));
    }
    if s.eq_ignore_ascii_case("NULL") {
        return Ok(Value::Null);
    }
    if numeric(s) {
        if s.len() > LIMIT {
            return Err(err(offset, line, "单值超过 16 MiB"));
        }
        return Ok(match s.parse::<i128>() {
            Ok(number) if i64::try_from(number).is_ok() => Value::Integer(s.into()),
            Ok(number) if u64::try_from(number).is_ok() => Value::Unsigned(s.into()),
            _ => Value::Decimal(s.into()),
        });
    }
    if s.as_bytes()
        .get(..2)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"0x"))
    {
        return Ok(Value::Bytes(STANDARD.encode(decode_hex_number(
            &s[2..],
            offset,
            line,
        )?)));
    }
    if s.as_bytes()
        .get(..2)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"0b"))
    {
        return decode_bits(&s[2..], offset, line);
    }
    if s.as_bytes()
        .first()
        .is_some_and(|b| b.eq_ignore_ascii_case(&b'n'))
        && s.as_bytes().get(1) == Some(&b'\'')
    {
        return literal(&s[1..], context, offset, line);
    }
    if s.len() >= 3
        && s.as_bytes()[1] == b'\''
        && s.ends_with('\'')
        && s.as_bytes()[0].eq_ignore_ascii_case(&b'x')
    {
        return Ok(Value::Bytes(STANDARD.encode(decode_hex(
            &s[2..s.len() - 1],
            offset,
            line,
        )?)));
    }
    if s.len() >= 3
        && s.as_bytes()[1] == b'\''
        && s.ends_with('\'')
        && s.as_bytes()[0].eq_ignore_ascii_case(&b'b')
    {
        return decode_bits(&s[2..s.len() - 1], offset, line);
    }
    let (charset, quoted) = if s.starts_with('_') {
        let end = s
            .as_bytes()
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, c)| !c.is_ascii_alphanumeric() && **c != b'_')
            .map(|(i, _)| i)
            .ok_or_else(|| err(offset, line, "非法字符集 introducer"))?;
        (Some(&s[1..end]), s[end..].trim_start())
    } else {
        (None, s)
    };
    if charset.is_some()
        && quoted.len() >= 3
        && quoted.as_bytes()[0].eq_ignore_ascii_case(&b'x')
        && quoted.as_bytes()[1] == b'\''
        && quoted.ends_with('\'')
    {
        return character_bytes(
            decode_hex(&quoted[2..quoted.len() - 1], offset, line)?,
            charset,
            offset,
            line,
        );
    }
    if charset.is_some()
        && quoted
            .as_bytes()
            .get(..2)
            .is_some_and(|p| p.eq_ignore_ascii_case(b"0x"))
    {
        return character_bytes(
            decode_hex_number(&quoted[2..], offset, line)?,
            charset,
            offset,
            line,
        );
    }
    let b = quoted.as_bytes();
    if b.len() < 2
        || !matches!(b[0], b'\'' | b'"')
        || b[b.len() - 1] != b[0]
        || (b[0] == b'"' && context.ansi_quotes)
    {
        return Err(err(offset, line, "INSERT 仅支持字面量，不支持表达式"));
    }
    let mut out = Vec::with_capacity(b.len() - 2);
    let mut i = 1;
    while i < b.len() - 1 {
        let c = b[i];
        i += 1;
        if c == b[0] {
            if i >= b.len() - 1 || b[i] != c {
                return Err(err(offset, line, "字符串引号错误"));
            }
            i += 1;
            out.push(c);
        } else if c == b'\\' && !context.no_backslash_escapes {
            if i >= b.len() - 1 {
                return Err(err(offset, line, "字符串转义不完整"));
            }
            let escaped = b[i];
            i += 1;
            out.push(match escaped {
                b'0' => 0,
                b'b' => 8,
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                b'Z' => 26,
                other => other,
            });
        } else {
            out.push(c);
        }
    }
    if charset.is_some() {
        let text =
            std::str::from_utf8(&out).map_err(|_| err(offset, line, "SQL 字符串解码错误"))?;
        let source = encoding_for(&context.input_encoding)?;
        let source = if source == UTF_16LE || source == UTF_16BE {
            if let Some(name) = context.character_set_client.as_deref() {
                encoding_for(name)?
            } else {
                match charset {
                    Some(name) if name.eq_ignore_ascii_case("binary") => UTF_8,
                    Some(name) => encoding_for(name)?,
                    None => UTF_8,
                }
            }
        } else {
            source
        };
        let (bytes, _, malformed) = source.encode(text);
        if malformed {
            return Err(err(offset, line, "SQL 字符串不能还原为输入字符集"));
        }
        return character_bytes(bytes.into_owned(), charset, offset, line);
    }
    character_bytes(out, None, offset, line)
}

pub(super) fn tuple_values(
    tuple: &[u8],
    context: &SqlContext,
    offset: u64,
    positions: &[(u64, u64)],
    line: u64,
) -> Result<Vec<Value>> {
    let s = std::str::from_utf8(tuple).map_err(|_| err(offset, line, "输入不是有效 UTF-8"))?;
    if !s.starts_with('(') || !s.ends_with(')') {
        return Err(err(offset, line, "tuple 未闭合"));
    }
    let bytes = s.as_bytes();
    let mut start = 1;
    let mut i = 1;
    let mut quote = 0;
    let mut values = Vec::new();
    while i < bytes.len() - 1 {
        let b = bytes[i];
        if quote != 0 {
            if b == b'\\' && !context.no_backslash_escapes {
                i += 2;
                continue;
            }
            if b == quote {
                if i + 1 < bytes.len() - 1 && bytes[i + 1] == quote {
                    i += 2;
                    continue;
                }
                quote = 0;
            }
        } else if matches!(b, b'\'' | b'"') {
            quote = b;
        } else if b == b',' {
            let (pos, line) = positions
                .get(values.len())
                .copied()
                .unwrap_or((offset, line));
            values.push(literal(&s[start..i], context, pos, line)?);
            start = i + 1;
        } else if b == b'(' || b == b')' {
            let (pos, line) = positions
                .get(values.len())
                .copied()
                .unwrap_or((offset, line));
            return Err(err(pos, line, "INSERT 不允许表达式"));
        }
        i += 1;
    }
    if quote != 0 {
        return Err(err(offset, line, "字符串未闭合"));
    }
    let (pos, line) = positions
        .get(values.len())
        .copied()
        .unwrap_or((offset, line));
    values.push(literal(&s[start..bytes.len() - 1], context, pos, line)?);
    Ok(values)
}
