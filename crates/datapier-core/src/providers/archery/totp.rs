//! 标准 TOTP 链接的本地校验和验证码生成。
use crate::model::{AppError, Result};
use std::time::{SystemTime, UNIX_EPOCH};
use totp_rs::{Algorithm, Builder, Secret, Totp};

fn invalid() -> AppError {
    // 库的解析错误可能包含输入内容，不能直接转发到 IPC 或日志。
    AppError::invalid("TOTP 链接或参数无效")
}

fn parse_totp(uri: &str) -> Result<Totp> {
    // URL 解析器会容忍部分非法转义和控制字符；先严格检查原始输入。
    let bytes = uri.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_control() || bytes[i] == b' ' {
            return Err(invalid());
        }
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return Err(invalid());
            }
            let hex = |byte: u8| match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => byte - b'A' + 10,
            };
            decoded.push(hex(bytes[i + 1]) * 16 + hex(bytes[i + 2]));
            i += 2;
        } else {
            decoded.push(bytes[i]);
        }
        i += 1;
    }
    if std::str::from_utf8(&decoded).is_err() || decoded.iter().any(u8::is_ascii_control) {
        return Err(invalid());
    }
    let url = reqwest::Url::parse(uri).map_err(|_| invalid())?;
    if url.scheme() != "otpauth"
        || url.host_str() != Some("totp")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
        || url.path().len() <= 1
        || url.path()[1..].contains('/')
    {
        return Err(invalid());
    }

    let mut secret = None;
    if url.query().is_none_or(|query| {
        query
            .split('&')
            .any(|pair| pair.split_once('=').is_none_or(|(key, _)| key.is_empty()))
    }) {
        return Err(invalid());
    }
    let mut algorithm = None;
    let mut digits = None;
    let mut period = None;
    let mut issuer = None;
    for (key, value) in url.query_pairs() {
        if key.contains('\u{fffd}') || value.contains('\u{fffd}') {
            return Err(invalid());
        }
        let slot = match key.as_ref() {
            "secret" => &mut secret,
            "algorithm" => &mut algorithm,
            "digits" => &mut digits,
            "period" => &mut period,
            "issuer" => &mut issuer,
            _ => return Err(invalid()),
        };
        if slot.replace(value.into_owned()).is_some() {
            return Err(invalid());
        }
    }
    if issuer.as_ref().is_some_and(|value| value.is_empty()) {
        return Err(invalid());
    }

    let encoded = secret.as_deref().ok_or_else(invalid)?;
    let unpadded = encoded.trim_end_matches('=');
    let padding = encoded.len() - unpadded.len();
    if unpadded.is_empty()
        || !unpadded
            .bytes()
            .all(|b| b.is_ascii_alphabetic() || (b'2'..=b'7').contains(&b))
        || (padding != 0 && (encoded.len() % 8 != 0 || padding != (8 - unpadded.len() % 8) % 8))
    {
        return Err(invalid());
    }
    let secret = Secret::try_from_base32(unpadded).map_err(|_| invalid())?;
    // 重新编码可拒绝无效长度和非零尾部位，避免库解码器容忍非规范编码。
    if !secret.to_base32().eq_ignore_ascii_case(unpadded) {
        return Err(invalid());
    }
    let algorithm = match algorithm.as_deref().unwrap_or("SHA1") {
        "SHA1" => Algorithm::SHA1,
        "SHA256" => Algorithm::SHA256,
        "SHA512" => Algorithm::SHA512,
        _ => return Err(invalid()),
    };
    let digits = match digits.as_deref().unwrap_or("6") {
        "6" => 6,
        "7" => 7,
        "8" => 8,
        _ => return Err(invalid()),
    };
    let period = match period {
        None => 30,
        Some(value) => {
            let parsed = value.parse::<u64>().map_err(|_| invalid())?;
            if parsed == 0 || parsed.to_string() != value {
                return Err(invalid());
            }
            parsed
        }
    };
    Builder::new()
        .with_secret(secret)
        .with_algorithm(algorithm)
        .with_digits(digits)
        .with_step_duration(period)
        .build()
        .map_err(|_| invalid())
}

/// 校验标准 TOTP 链接；错误不包含链接或秘密。
pub fn validate_totp_uri(uri: &str) -> Result<()> {
    parse_totp(uri).map(|_| ())
}

/// 使用当前系统时间生成验证码，保留前导零；错误不包含链接或秘密。
pub fn generate_totp(uri: &str) -> Result<String> {
    let totp = parse_totp(uri)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AppError::new("clock", "系统时间早于 Unix 纪元"))?
        .as_secs();
    Ok(totp.generate(now).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> String {
        Secret::from("12345678901234567890".as_bytes()).to_base32()
    }

    #[test]
    fn rfc6238_vectors_and_defaults() {
        let secret = secret();
        let uri = format!("otpauth://totp/example?secret={secret}&digits=8");
        let totp = parse_totp(&uri).unwrap();
        assert_eq!(totp.generate(59).to_string(), "94287082");
        assert_eq!(totp.generate(1_111_111_109).to_string(), "07081804");
        let default = parse_totp(&format!("otpauth://totp/example?secret={secret}")).unwrap();
        assert_eq!(default.generate(59).to_string(), "287082");
        let current = generate_totp(&uri).unwrap();
        assert_eq!(current.len(), 8);
        assert!(current.bytes().all(|b| b.is_ascii_digit()));
    }

    #[test]
    fn supports_sha256_sha512_and_period() {
        for (algorithm, raw, expected) in [
            ("SHA256", "12345678901234567890123456789012", "46119246"),
            (
                "SHA512",
                "1234567890123456789012345678901234567890123456789012345678901234",
                "90693936",
            ),
        ] {
            let encoded = Secret::from(raw.as_bytes()).to_base32();
            let uri = format!(
                "otpauth://totp/example?secret={encoded}&issuer=Example&algorithm={algorithm}&digits=8&period=30"
            );
            assert_eq!(parse_totp(&uri).unwrap().generate(59).to_string(), expected);
        }
        let uri = format!("otpauth://totp/example?secret={}&period=60", secret());
        assert_eq!(parse_totp(&uri).unwrap().generate(59).to_string(), "755224");
    }

    #[test]
    fn rejects_invalid_uri_parameters_and_base32_without_disclosure() {
        let secret = secret();
        let cases = [
            format!("otpauth://hotp/example?secret={secret}&counter=1"),
            format!("otpauth://totp/example?secret={secret}&secret={secret}"),
            format!("otpauth://totp/example?secret={secret}&other=1"),
            format!("otpauth://totp/example?secret={secret}&algorithm=MD5"),
            format!("otpauth://totp/example?secret={secret}&digits=9"),
            format!("otpauth://totp/example?secret={secret}&period=0"),
            format!("otpauth://totp/example?secret={secret}&period=+30"),
            format!("otpauth://totp/example?secret={secret}%"),
            format!("otpauth://totp/example?secret={secret}#fragment"),
            "otpauth://totp/example?secret=INVALID1".into(),
            "otpauth://totp/example?secret=MY======".into(),
            "otpauth://totp/example?secret=ABC".into(),
        ];
        for uri in cases {
            let error = validate_totp_uri(&uri).unwrap_err().to_string();
            assert!(!error.contains(&uri));
            assert!(!error.contains(&secret));
            assert!(generate_totp(&uri).is_err());
        }
    }
}
