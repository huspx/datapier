use super::*;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use datapier_core::model::Value as V;

fn find_class<'a>(node: &'a Node<Msg>, class: &str) -> Option<&'a Node<Msg>> {
    if node.props().get("class").and_then(|value| value.as_str()) == Some(class) {
        return Some(node);
    }
    node.child_nodes()
        .iter()
        .find_map(|child| find_class(child, class))
}

#[test]
fn schema_view_preserves_comments_and_empty_values() {
    let mut app = App::new("test".into());
    let comment = "字段\n第二行 {value} 和中文";
    app.schema = Some(json!({
        "columns": [
            {"name": "with_comment", "comment": comment},
            {"name": "null_comment", "comment": null},
            {"name": "empty_comment", "comment": ""},
            {"name": "missing_comment"}
        ]
    }));
    let view = View::new().main(vec![schema_panel(&app)]);
    let root = view.regions()[0].unwrap();
    let panel = find_class(root, "schema-panel").unwrap();
    let comments: Vec<_> = panel
        .child_nodes()
        .iter()
        .filter_map(|row| find_class(row, "schema-comment"))
        .map(|comment| {
            comment
                .props()
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
        })
        .collect();
    assert_eq!(comments, [comment, "", "", ""]);
}

#[test]
fn tree_pages_preserve_all_visible_keys_without_unbounded_views() {
    let mut app = App::new("test".into());
    app.workbench.tree = true;
    app.resources = (0..450)
        .map(|index| datapier_core::model::Resource {
            name: format!("flat{index:03}"),
            kind: "string".into(),
            database: None,
            key_b64: None,
            data: Value::Null,
        })
        .collect();
    fn labels(node: &Node<Msg>, output: &mut Vec<String>) {
        if node.props().get("class").and_then(Value::as_str) == Some("key-name") {
            output.push(node.props()["text"].as_str().unwrap().to_owned());
        }
        for child in node.child_nodes() {
            labels(child, output);
        }
    }
    let (backend, _, _) = crate::backend::Backend::scripted();
    for page in 0..3 {
        let view = View::new().main(vec![redis_browser(&app)]);
        let mut actual = Vec::new();
        labels(view.regions()[0].unwrap(), &mut actual);
        let start = page * workbench::KEY_PAGE_SIZE;
        let end = (start + workbench::KEY_PAGE_SIZE).min(450);
        let expected: Vec<_> = (start..end)
            .map(|index| format!("flat{index:03}"))
            .collect();
        assert_eq!(actual, expected);
        app.workbench_action("keys-next", &backend);
    }
    assert_eq!(app.workbench.key_page, 2);
}

#[test]
fn cli_reply_preserves_unicode_binary_and_large_integer() {
    let reply = V::Json(json!([
        {"type":"bytes","value":STANDARD.encode("中文😀")},
        {"type":"bytes","value":STANDARD.encode([0xff, 0x00])},
        {"type":"integer","value":"9223372036854775807"},
        {"type":"missing"}
    ]));
    assert_eq!(
        value_text(&reply, true),
        "1) 中文😀\n2) base64:/wA=\n3) 9223372036854775807\n4) (nil)"
    );
}
