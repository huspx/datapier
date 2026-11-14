use datapier_core::model::Value;
use datapier_core::value::{
    ValueBuffer, ValueKind, format_json, format_xml, from_bytes, parse_hex, preview_image,
    to_bytes, validate_json, validate_xml,
};

#[test]
fn bytes_and_null_remain_distinct() {
    let raw = [0, 255, 10, 13];
    let value = from_bytes(ValueKind::Bytes, Some(&raw)).unwrap();
    assert_eq!(to_bytes(&value).unwrap().1.unwrap(), raw);
    assert_eq!(
        from_bytes(ValueKind::Bytes, Some(&[])).unwrap(),
        Value::Bytes(String::new())
    );
    assert_eq!(from_bytes(ValueKind::Null, None).unwrap(), Value::Null);
    assert!(from_bytes(ValueKind::Null, Some(&[])).is_err());
    assert!(from_bytes(ValueKind::Bytes, None).is_err());
}

#[test]
fn mysql_time_preserves_negative_and_long_durations() {
    let raw = b"-838:59:59.123456";
    assert_eq!(
        from_bytes(ValueKind::Time, Some(raw)).unwrap(),
        Value::Time("-838:59:59.123456".into())
    );
    assert!(from_bytes(ValueKind::Time, Some(b"839:00:00")).is_err());
    assert!(from_bytes(ValueKind::Time, Some(b"12:60:00")).is_err());
}
#[test]
fn patch_is_atomic_and_checks_revision_and_utf8_boundaries() {
    let mut buffer = ValueBuffer::new(&Value::Text("a中文😀".into()), 20).unwrap();
    assert!(buffer.patch(0, 2..3, b"x", true).is_err());
    assert_eq!(buffer.revision(), 0);
    assert_eq!(buffer.patch(0, 1..4, "好".as_bytes(), true).unwrap(), 1);
    assert!(buffer.patch(0, 0..1, b"z", true).is_err());
    assert!(buffer.patch(1, 0..1, &[0; 20], false).is_err());
    assert_eq!(buffer.value().unwrap(), Value::Text("a好文😀".into()));
    assert_eq!(buffer.read(1, 3).unwrap(), "好".as_bytes());
}

#[test]
fn hex_requires_complete_pairs() {
    assert_eq!(parse_hex("00 ff\n12", 3).unwrap(), [0, 255, 18]);
    assert!(parse_hex("0 f", 3).is_err());
    assert!(parse_hex("00 zz", 3).is_err());
    assert!(parse_hex("001", 3).is_err());
    assert!(parse_hex("0000", 1).is_err());
}

#[test]
fn json_precision_order_and_duplicate_keys() {
    let input = r#"{"z":123456789012345678901234567890,"a":0.1234567890123456789012345}"#;
    let formatted = format_json(input, 1024).unwrap();
    assert!(formatted.contains("123456789012345678901234567890"));
    assert!(formatted.contains("0.1234567890123456789012345"));
    assert!(formatted.find("\"z\"").unwrap() < formatted.find("\"a\"").unwrap());
    let repeated = r#"{"x":1,"x":2}"#;
    assert!(validate_json(repeated).is_ok());
    assert!(format_json(repeated, 1024).is_err());
    assert!(from_bytes(ValueKind::Json, Some(repeated.as_bytes())).is_err());
}

#[test]
fn xml_rejects_unsafe_formatting_and_invalid_documents() {
    assert!(validate_xml("<a/><b/>").is_err());
    assert!(validate_xml("<p:a/>").is_err());
    assert!(validate_xml("<a x='1' x='2'/>").is_err());
    assert!(validate_xml("<!DOCTYPE a><a/>").is_err());
    assert!(validate_xml("<a>&unknown;</a>").is_err());
    let mixed = "<a>before<b/>after</a>";
    validate_xml(mixed).unwrap();
    assert!(format_xml(mixed, 1024).is_err());
    assert!(format_xml("<a xml:space='preserve'><b/></a>", 1024).is_err());
    let formatted = format_xml("<?xml version='1.0'?><a><b/><c/></a>", 1024).unwrap();
    assert!(formatted.contains("\n  <b/>\n  <c/>\n"));
    validate_xml(&formatted).unwrap();
}

#[test]
fn xml_deep_indentation_obeys_output_limit() {
    let input = format!("{}<leaf/>{}", "<a>".repeat(2000), "</a>".repeat(2000));
    let error = format_xml(&input, 64 * 1024).unwrap_err();
    assert_eq!(error.code, "resource_limit");
    let input = "<a>       <b/>       </a>";
    let formatted = format_xml(input, 20).unwrap();
    assert_eq!(formatted, "<a>\n  <b/>\n</a>");
}

#[test]
fn invalid_and_unsafe_media_are_rejected() {
    assert!(preview_image(b"\x89PNG\r\n\x1a\ninvalid", 1024 * 1024).is_err());
    assert!(
        preview_image(
            br#"<svg xmlns="http://www.w3.org/2000/svg"><image href="file:///etc/passwd"/></svg>"#,
            1024 * 1024
        )
        .is_err()
    );
    assert!(
        preview_image(
            br#"<svg xmlns="http://www.w3.org/2000/svg"><script/></svg>"#,
            1024 * 1024
        )
        .is_err()
    );
    assert!(
        preview_image(
            br#"<svg xmlns="http://www.w3.org/2000/svg"><filter id="f"/></svg>"#,
            1024 * 1024
        )
        .is_err()
    );
    assert!(
        preview_image(
            br#"<svg xmlns="http://www.w3.org/2000/svg" width="4097" height="1"/>"#,
            1024 * 1024
        )
        .is_err()
    );
    let valid = br#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="red"/></svg>"#;
    let preview = preview_image(valid, 1024 * 1024).unwrap();
    assert_eq!((preview.width, preview.height), (8, 8));
    assert!(preview.png.starts_with(b"\x89PNG\r\n\x1a\n"));
}

#[test]
fn raster_preview_roundtrips_pixels_and_respects_budget() {
    use image::ImageEncoder;
    let mut source = Vec::new();
    image::codecs::png::PngEncoder::new(&mut source)
        .write_image(&[12, 34, 56, 255], 1, 1, image::ExtendedColorType::Rgba8)
        .unwrap();
    assert_eq!(
        preview_image(&source, source.len()).unwrap_err().code,
        "resource_limit"
    );
    let preview = preview_image(&source, 4096).unwrap();
    let decoded = image::load_from_memory(&preview.png).unwrap().into_rgba8();
    assert_eq!(decoded.as_raw(), &[12, 34, 56, 255]);
}

#[test]
#[ignore = "需要系统字体，随原生媒体场景显式运行"]
fn svg_text_has_visible_ink() {
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="180" height="60"><rect width="180" height="60" fill="white"/><text x="8" y="40" font-size="32" font-family="sans-serif" fill="black">Hello</text></svg>"#;
    let preview = preview_image(svg, 16 * 1024 * 1024).unwrap();
    let image = image::load_from_memory(&preview.png).unwrap().to_rgba8();
    let ink = image
        .pixels()
        .filter(|p| p[3] > 0 && p[0] < 128 && p[1] < 128 && p[2] < 128)
        .count();
    assert!(ink > 100, "SVG text disappeared: {ink} ink pixels");
}
