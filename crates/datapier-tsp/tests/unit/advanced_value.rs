use super::*;

#[test]
fn preview_handle_survives_until_png_is_complete_then_is_retired() {
    let mut ui = ValueUi {
        revision: 7,
        mode: Mode::Image,
        ..ValueUi::default()
    };
    let request = ui
        .image_response(
            &json!({"preview_id":"derived","revision":7,"mime":"image/png","total_bytes":5}),
        )
        .unwrap();
    assert_eq!(request.1["value_session"], "derived");
    let next = ui.read_response(&json!({"value_session":"derived","revision":7,"offset":0,"data_b64":STANDARD.encode(b"abc")})).unwrap();
    assert!(next.is_some());
    assert!(ui.take_preview_png().is_none());
    assert!(ui.retired_previews.is_empty());
    ui.read_response(&json!({"value_session":"derived","revision":7,"offset":3,"data_b64":STANDARD.encode(b"de")})).unwrap();
    assert_eq!(ui.take_preview_png().unwrap(), b"abcde");
    assert_eq!(ui.retired_previews, vec![("derived".into(), 7)]);
    ui.image_response(
        &json!({"preview_id":"second","revision":7,"mime":"image/png","total_bytes":1}),
    )
    .unwrap();
    ui.invalidate_preview();
    assert_eq!(
        ui.retired_previews,
        vec![("derived".into(), 7), ("second".into(), 7)]
    );
}

#[test]
fn small_pages_reassemble_text_without_losing_bytes() {
    let mut ui = ValueUi {
        max_text_bytes: 8192,
        ..ValueUi::default()
    };
    let data = "中🙂abc".repeat(400).into_bytes();
    let first = 517;
    let mut request = ui.summary_response(&json!({"value_session":"session","revision":1,"total_bytes":data.len(),"writable":true,"kind":"text","is_null":false,"offset":0,"data_b64":STANDARD.encode(&data[..first])})).unwrap();
    while let Some((method, params)) = request {
        assert_eq!(method, "value.read");
        let offset = params["offset"].as_u64().unwrap() as usize;
        let end = (offset + 517).min(data.len());
        request = ui.read_response(&json!({"value_session":"session","revision":1,"offset":offset,"data_b64":STANDARD.encode(&data[offset..end])})).unwrap();
    }
    assert_eq!(ui.original_window, data);
}
