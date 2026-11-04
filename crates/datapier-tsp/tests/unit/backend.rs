use super::*;
use std::time::Duration;

impl Backend {
    pub(crate) fn scripted() -> (Self, mpsc::Receiver<Request>, mpsc::Sender<Response>) {
        let (commands, incoming) = mpsc::channel(64);
        let (responses, receiver) = mpsc::channel(64);
        (
            Self {
                commands,
                responses: receiver,
                thread: None,
            },
            incoming,
            responses,
        )
    }
}

#[test]
fn grid_large_value_requires_complete_matching_typed_json() {
    let text = "x".repeat(300_000);
    let bytes = serde_json::to_vec(&DataValue::Text(text.clone())).unwrap();
    let full = decode_large(&bytes, "text").unwrap();
    assert_eq!(
        crate::grid::encode_tsv(&[vec![full]]).unwrap(),
        format!("{text}\n")
    );
    assert!(decode_large(&bytes[..bytes.len() - 1], "text").is_err());
    assert!(decode_large(&bytes, "bytes").is_err());
}

#[test]
fn redis_copy_preserves_strings_and_structured_bytes() {
    let value = |bytes: &[u8]| DataValue::Bytes(STANDARD.encode(bytes));
    for raw in [b"".as_slice(), "中文".as_bytes(), b"base64:AA==".as_slice()] {
        assert_eq!(
            redis_copy_encode("string", vec![vec![value(raw)]])
                .unwrap()
                .as_bytes(),
            raw
        );
    }
    assert_eq!(
        redis_copy_encode("string", vec![vec![value(&[255, 0])]]).unwrap(),
        "base64:/wA="
    );
    let large = vec![b'x'; 1_000_000];
    assert_eq!(
        redis_copy_encode("string", vec![vec![value(&large)]])
            .unwrap()
            .as_bytes(),
        large
    );
    let hash: Value = serde_json::from_str(
        &redis_copy_encode("hash", vec![vec![value(b"base64:x"), value(&[255])]]).unwrap(),
    )
    .unwrap();
    assert_eq!(
        hash["entries"][0]["field_b64"],
        STANDARD.encode(b"base64:x")
    );
    assert_eq!(hash["entries"][0]["value_b64"], "/w==");
    let list: Value = serde_json::from_str(
        &redis_copy_encode(
            "list",
            vec![vec![
                DataValue::Integer("9007199254740993".into()),
                value(b""),
            ]],
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(list["entries"][0]["index"], "9007199254740993");
    let set: Value =
        serde_json::from_str(&redis_copy_encode("set", vec![vec![value(&[0])]]).unwrap()).unwrap();
    assert_eq!(set["entries"][0]["member_b64"], "AA==");
    let zset: Value = serde_json::from_str(
        &redis_copy_encode(
            "zset",
            vec![vec![
                value(b"m"),
                DataValue::Float("1.0000000000000002".into()),
            ]],
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(zset["entries"][0]["score"], "1.0000000000000002");
    assert!(redis_copy_encode("string", vec![]).is_err());
}

#[test]
fn selected_rows_preserve_binary_precision_and_infinite_scores() {
    let bytes = DataValue::Bytes(STANDARD.encode([255, 0]));
    let rows = vec![vec![
        DataValue::Integer("9007199254740993".into()),
        bytes.clone(),
    ]];
    let list: Value =
        serde_json::from_str(&redis_copy_selected_encode("list", rows).unwrap()).unwrap();
    assert_eq!(
        list["entries"][0]["index"],
        json!({"type":"integer","value":"9007199254740993"})
    );
    assert_eq!(
        list["entries"][0]["value"],
        json!({"type":"bytes","value":"/wA="})
    );
    let rows = ["inf", "-inf", "1.0000000000000002"]
        .into_iter()
        .map(|score| vec![bytes.clone(), DataValue::Float(score.into())])
        .collect();
    let zset: Value =
        serde_json::from_str(&redis_copy_selected_encode("zset", rows).unwrap()).unwrap();
    for (entry, score) in
        zset["entries"]
            .as_array()
            .unwrap()
            .iter()
            .zip(["inf", "-inf", "1.0000000000000002"])
    {
        assert_eq!(entry["score"], json!({"type":"float","value":score}));
        assert_eq!(entry["member"], json!({"type":"bytes","value":"/wA="}));
    }
    assert!(redis_copy_selected_encode("set", vec![vec![DataValue::Bytes("!".into())]]).is_err());
}

#[test]
fn closed_events_end_consumer_and_allow_unsaved_workspace_to_fail() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (commands, mut incoming) = mpsc::channel(1);
    let (responses, receiver) = mpsc::channel(1);
    let (event_sender, mut events) = broadcast::channel(1);
    let mut backend = Backend {
        commands,
        responses: receiver,
        thread: None,
    };
    drop(event_sender);
    runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(1),
            consume(&mut incoming, &mut events, &responses, |_| {}, || {}),
        )
        .await
        .expect("event closure must stop the consumer while commands remain open");
    });
    drop(responses);

    let mut app = crate::App::new("test".into());
    app.dirty = true;
    let error = crate::workspace::finish_after_input_closed(&mut app, &mut backend).unwrap_err();
    assert_eq!(error.code, "workspace_save");
}
