use super::*;

#[test]
fn preview_budget_preserves_lengths_and_caps_entire_page() {
    let mut budget = PreviewBudget::new();
    let input = "汉\n".repeat(1000);
    let first = budget.parameter("text", input.as_bytes(), false);
    assert_eq!(first.byte_len, Some(input.len()));
    assert!(!first.complete);
    assert!(first.display.ends_with('…'));
    assert!(first.display.len() <= 1024);
    let mut used = first.display.len();
    for _ in 0..1000 {
        let parameter = budget.parameter("bytes", &[255; 2048], false);
        assert_eq!(parameter.byte_len, Some(2048));
        assert!(!parameter.complete);
        used += parameter.display.len();
    }
    assert!(used <= 512 * 1024);
}

#[test]
fn preview_rejects_oversized_metadata() {
    let preview = ChangePreview {
        provider: "mysql",
        transactional: true,
        total_changes: 1,
        offset: 0,
        next_offset: None,
        notes: Vec::new(),
        items: vec![PreviewItem {
            index: Some(0),
            operation: "insert".into(),
            statements: vec![PreviewStatement {
                role: "write",
                text: "x".repeat(8 * 1024 * 1024),
                parameters: Vec::new(),
            }],
        }],
    };
    assert_eq!(preview_json(preview).unwrap_err().code, "result_too_large");
}
