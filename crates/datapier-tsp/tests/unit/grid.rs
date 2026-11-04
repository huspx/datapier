use super::*;
use datapier_core::model::{ResultMeta, ResultPage};
use uuid::Uuid;

fn grid(columns: Vec<Column>, rows: Vec<Vec<Value>>) -> GridState {
    GridState::new(
        ResultPage {
            result: ResultMeta {
                id: Uuid::new_v4(),
                execution_id: Uuid::new_v4(),
                columns,
                kind: "rows".into(),
                row_count: rows.len() as u64,
                complete: true,
                summary: serde_json::Value::Null,
            },
            rows,
            start: 10,
            next: None,
        },
        true,
    )
}

#[test]
fn mysql_fit_tracks_page_limits_and_hidden_frozen_columns() {
    let mut g = grid(
        vec![
            Column::new("a", "text"),
            Column::new("b", "text"),
            Column::new("c", "text"),
        ],
        vec![vec![
            Value::Text("x".into()),
            Value::Text("y".into()),
            Value::Text("z".into()),
        ]],
    );
    g.fit_columns(2000, true);
    assert!(g.settings.iter().all(|s| s.width == 480));
    g.settings[0].hidden = true;
    g.settings[0].frozen = true;
    g.settings[1].frozen = true;
    g.settings[2].frozen = true;
    g.fit_columns(200, true);
    assert_eq!(g.frozen_offset(1), Some(0));
    assert_eq!(g.frozen_offset(2), Some(u32::from(g.settings[1].width)));
    g.fit_columns(10, true);
    assert!(g.settings[1..].iter().all(|s| s.width >= 64));
    let mut page = g.page.clone();
    page.rows[0][1] = Value::Text("x".repeat(100));
    g.set_page(page).unwrap();
    g.fit_columns(10, true);
    assert_eq!(g.settings[1].width, 480);
    g.fit_columns(2000, false);
    assert!(g.settings[1].width > 480);
}

#[test]
fn selection_copies_rectangle_with_null_and_binary_losslessly() {
    let mut g = grid(
        vec![Column::new("a", "text"), Column::new("b", "bytes")],
        vec![
            vec![Value::Null, Value::Bytes("AP8=".into())],
            vec![Value::Text("\\N".into()), Value::Bytes("YQ==".into())],
        ],
    );
    g.select(Cell { row: 10, column: 0 }, false).unwrap();
    assert_eq!(g.copy_tsv().unwrap(), "\\N\n");
    g.select(Cell { row: 11, column: 1 }, true).unwrap();
    assert_eq!(g.copy_tsv().unwrap(), "\\N\tAP8=\n\\\\N\tYQ==\n");
    g.select(Cell { row: 11, column: 1 }, false).unwrap();
    assert_eq!(g.copy_tsv().unwrap(), "YQ==\n");
}

#[test]
fn row_selection_respects_visible_rows_and_prunes_on_filter_change() {
    let mut grid = grid(
        vec![Column::new("value", "text")],
        vec![
            vec![Value::Text("match 0".into())],
            vec![Value::Text("hidden".into())],
            vec![Value::Text("match 2".into())],
        ],
    );
    grid.page.start = 0;
    grid.set_visible_rows([0, 2]);
    grid.select_row(0, 0, false, false).unwrap();
    grid.select_row(2, 0, true, false).unwrap();
    assert_eq!(grid.selected_rows(), vec![0, 2]);
    assert_eq!(grid.copy_tsv().unwrap(), "match 0\nmatch 2\n");

    grid.set_visible_rows([1, 2]);
    assert_eq!(grid.selected_rows(), vec![2]);
    grid.select_row(1, 0, true, false).unwrap();
    assert_eq!(grid.selected_rows(), vec![1]);
    grid.set_visible_rows([]);
    assert!(grid.selected_rows().is_empty());
    assert!(grid.focus.is_none());
}

#[test]
fn keyboard_navigation_skips_filtered_rows_and_stays_on_visible_boundaries() {
    let mut grid = grid(
        vec![Column::new("a", "text"), Column::new("b", "text")],
        vec![vec![Value::Null, Value::Null]; 5],
    );
    grid.set_visible_rows([11, 13]);
    grid.navigate(Navigation::Down, false);
    assert_eq!(grid.focus, Some(Cell { row: 13, column: 0 }));
    grid.navigate(Navigation::Down, false);
    assert_eq!(grid.focus, Some(Cell { row: 13, column: 0 }));
    grid.navigate(Navigation::Up, true);
    assert_eq!(grid.focus, Some(Cell { row: 11, column: 0 }));
    grid.navigate(Navigation::Up, false);
    assert_eq!(grid.focus, Some(Cell { row: 11, column: 0 }));

    grid.navigate(Navigation::End, false);
    grid.navigate(Navigation::Tab, false);
    assert_eq!(grid.focus, Some(Cell { row: 13, column: 0 }));
    grid.navigate(Navigation::BackTab, false);
    assert_eq!(grid.focus, Some(Cell { row: 11, column: 1 }));
    grid.set_visible_rows([13]);
    grid.navigate(Navigation::BackTab, false);
    assert_eq!(grid.focus, Some(Cell { row: 13, column: 0 }));
    grid.set_visible_rows([]);
    grid.navigate(Navigation::Down, false);
    assert!(grid.focus.is_none());
}

#[test]
fn table_column_merge_requires_unique_complete_names_and_enforces_constraints() {
    let mut grid = grid(
        vec![
            Column::new("computed", "display int"),
            Column::new("required", "display text"),
        ],
        vec![vec![Value::Integer("1".into()), Value::Text("x".into())]],
    );
    let mut computed = Column::new("computed", "int");
    computed.generated = true;
    let mut required = Column::new("required", "varchar(20)");
    required.nullable = false;
    required.primary_key = true;
    required.auto_increment = true;
    required.default_value = Some("default".into());

    assert!(merge_table_columns(&mut grid.page, &[computed.clone()]).is_err());
    assert!(merge_table_columns(&mut grid.page, &[computed.clone(), computed.clone()]).is_err());
    assert!(
        merge_table_columns(
            &mut grid.page,
            &[computed.clone(), Column::new("other", "int")]
        )
        .is_err()
    );
    assert!(!grid.page.result.columns[0].generated);
    let mut duplicate_page = grid.page.clone();
    duplicate_page.result.columns[1].name = "computed".into();
    assert!(
        merge_table_columns(&mut duplicate_page, &[computed.clone(), required.clone()]).is_err()
    );
    assert!(!duplicate_page.result.columns[0].generated);

    merge_table_columns(&mut grid.page, &[required, computed]).unwrap();
    assert_eq!(grid.page.result.columns[0].type_name, "display int");
    let column = &grid.page.result.columns[1];
    assert_eq!(column.type_name, "display text");
    assert!(!column.nullable && column.primary_key && column.auto_increment);
    assert_eq!(column.default_value.as_deref(), Some("default"));
    assert!(
        grid.parse_cell_text(Cell { row: 10, column: 0 }, "2")
            .is_err()
    );
    assert!(
        grid.parse_cell_text(Cell { row: 10, column: 1 }, "\\N")
            .is_err()
    );
}

#[test]
fn nullable_enum_and_set_edits_preserve_text_members() {
    let columns = vec![
        Column::new("id", "int"),
        Column::new("binary_choice", "enum('binary','普通')"),
        Column::new("json_choice", "ENUM('json','中文')"),
        Column::new("time_choices", "set('datetime','timestamp','date')"),
    ];
    let mut grid = grid(
        columns.clone(),
        vec![vec![
            Value::Integer("1".into()),
            Value::Null,
            Value::Null,
            Value::Null,
        ]],
    );
    grid.select(Cell { row: 10, column: 1 }, false).unwrap();
    let preview = grid
        .preview_paste("普通\t中文\tdatetime,timestamp")
        .unwrap();
    grid.apply_paste(preview).unwrap();
    let changes = grid.changes(&["id".into()]).unwrap();
    assert_eq!(
        changes[0].values["binary_choice"],
        Value::Text("普通".into())
    );
    assert_eq!(changes[0].values["json_choice"], Value::Text("中文".into()));
    assert_eq!(
        changes[0].values["time_choices"],
        Value::Text("datetime,timestamp".into())
    );
    assert_eq!(
        decode_form_value("中文\\原文", false, &columns[2]).unwrap(),
        Value::Text("中文\\原文".into())
    );
}

#[test]
fn nullable_typed_columns_keep_their_editing_behavior() {
    for (ty, input, expected) in [
        ("VARBINARY(8)", "AP8=", Value::Bytes("AP8=".into())),
        ("mediumblob", "AP8=", Value::Bytes("AP8=".into())),
        (
            "json",
            "{\"x\":1}",
            Value::Json(serde_json::json!({"x": 1})),
        ),
        (
            "datetime(6)",
            "2026-01-02 03:04:05",
            Value::DateTime("2026-01-02 03:04:05".into()),
        ),
        (
            "timestamp(3)",
            "2026-01-02 03:04:05",
            Value::DateTime("2026-01-02 03:04:05".into()),
        ),
        ("date", "2026-01-02", Value::Date("2026-01-02".into())),
        ("integer unsigned", "42", Value::Unsigned("42".into())),
    ] {
        assert_eq!(
            decode_form_value(input, false, &Column::new("value", ty)).unwrap(),
            expected
        );
    }
    assert!(decode_form_value("not base64!", false, &Column::new("value", "binary(8)")).is_err());
    assert!(decode_form_value("not json", false, &Column::new("value", "json")).is_err());
}

#[test]
fn paste_respects_disjoint_rows_and_reverse_cell_selection() {
    let mut grid = grid(
        vec![Column::new("a", "int"), Column::new("b", "int")],
        (0..4)
            .map(|n| vec![Value::Integer(n.to_string()); 2])
            .collect(),
    );
    grid.select_row(10, 0, false, false).unwrap();
    grid.select_row(12, 0, false, true).unwrap();
    assert!(grid.preview_paste("7\n8\n9").is_err());
    let preview = grid.preview_paste("7\n8").unwrap();
    grid.apply_paste(preview).unwrap();
    assert_eq!(
        grid.value(Cell { row: 10, column: 0 }),
        Some(&Value::Integer("7".into()))
    );
    assert_eq!(
        grid.value(Cell { row: 11, column: 0 }),
        Some(&Value::Integer("1".into()))
    );
    assert_eq!(
        grid.value(Cell { row: 12, column: 0 }),
        Some(&Value::Integer("8".into()))
    );

    grid.select(Cell { row: 13, column: 1 }, false).unwrap();
    grid.select(Cell { row: 12, column: 0 }, true).unwrap();
    assert!(grid.preview_paste("1\t2\n3\t4\n5\t6").is_err());
    let preview = grid.preview_paste("1\t2\n3\t4").unwrap();
    grid.apply_paste(preview).unwrap();
    assert_eq!(
        grid.value(Cell { row: 12, column: 0 }),
        Some(&Value::Integer("1".into()))
    );
    assert_eq!(
        grid.value(Cell { row: 13, column: 1 }),
        Some(&Value::Integer("4".into()))
    );
}

#[test]
fn row_ranges_and_toggles_keep_original_page_order() {
    let mut grid = grid(
        vec![Column::new("id", "int")],
        (0..5)
            .map(|n| vec![Value::Integer(n.to_string())])
            .collect(),
    );
    grid.select_row(11, 0, false, false).unwrap();
    grid.select_row(13, 0, true, false).unwrap();
    grid.select_row(12, 0, false, true).unwrap();
    grid.select_row(10, 0, false, true).unwrap();
    assert_eq!(grid.selected_rows(), vec![10, 11, 13]);
    assert_eq!(grid.selected_row_count(), 3);
    assert!(grid.is_row_selected(13));
    assert!(!grid.is_row_selected(12));
    grid.edit(Cell { row: 11, column: 0 }, Value::Integer("99".into()))
        .unwrap();
    assert_eq!(grid.selected_rows(), vec![10, 11, 13]);
    assert!(grid.select_row(15, 0, false, false).is_err());
    grid.select(Cell { row: 11, column: 0 }, false).unwrap();
    assert_eq!(grid.selected_row_count(), 0);
}

#[test]
fn insert_rejects_incomplete_batches_and_missing_schema() {
    let columns = vec![Column::new("value", "text")];
    let mut grid = grid(
        columns.clone(),
        vec![
            vec![Value::Text("完整".into())],
            vec![Value::Large {
                id: Uuid::new_v4(),
                original_type: "text".into(),
                bytes: 99,
                preview: "截断".into(),
            }],
        ],
    );
    grid.select_row(10, 0, false, false).unwrap();
    grid.select_row(11, 0, false, true).unwrap();
    assert!(grid.copy_insert("db", "t", &columns, false).is_err());
    assert!(grid.copy_insert("db", "t", &[], false).is_err());
    grid.select_row(11, 0, false, true).unwrap();
    assert!(grid.copy_insert("db", "t", &columns, false).is_ok());
}

#[test]
fn insert_column_filter_preserves_other_composite_key_columns() {
    let mut auto = Column::new("id", "bigint unsigned");
    auto.primary_key = true;
    auto.auto_increment = true;
    let mut key = Column::new("tenant", "int");
    key.primary_key = true;
    let mut generated = Column::new("computed", "int");
    generated.generated = true;
    let columns = vec![auto, key, generated];
    let mut grid = grid(
        columns.clone(),
        vec![vec![
            Value::Unsigned("18446744073709551615".into()),
            Value::Integer("7".into()),
            Value::Integer("42".into()),
        ]],
    );
    grid.select_row(10, 0, false, false).unwrap();
    let sql = grid.copy_insert("db", "t", &columns, true).unwrap();
    let parsed =
        sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::MySqlDialect {}, &sql).unwrap();
    let sqlparser::ast::Statement::Insert(insert) = &parsed[0] else {
        panic!("不是 INSERT")
    };
    assert_eq!(insert.columns.len(), 1);
    assert!(
        matches!(&insert.columns[0].0[..], [sqlparser::ast::ObjectNamePart::Identifier(name)] if name.value == "tenant")
    );
}

#[test]
fn literal_encodings_preserve_null_bytes_json_and_precision() {
    fn decoded_hex(sql: &str) -> Vec<u8> {
        let start = sql.find("X'").unwrap() + 2;
        let hex = &sql[start..sql[start..].find('\'').unwrap() + start];
        hex.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    assert_eq!(mysql_literal(&Value::Null).unwrap(), "NULL");
    let bytes = vec![0, 0xff, b'\\', b'\'', 0x80];
    let sql = mysql_literal(&Value::Bytes(STANDARD.encode(&bytes))).unwrap();
    assert_eq!(decoded_hex(&sql), bytes);
    let text = "中文😀'\\\0";
    assert_eq!(
        decoded_hex(&mysql_literal(&Value::Text(text.into())).unwrap()),
        text.as_bytes()
    );
    let number = "123456789012345678901234567890.12345678901234567890";
    assert_eq!(
        mysql_literal(&Value::Decimal(number.into())).unwrap(),
        number
    );
    let json: serde_json::Value =
        serde_json::from_str("{\"n\":123456789012345678901234567890.123456789}").unwrap();
    let encoded = mysql_literal(&Value::Json(json.clone())).unwrap();
    let decoded: serde_json::Value = serde_json::from_slice(&decoded_hex(&encoded)).unwrap();
    assert_eq!(decoded, json);
}
