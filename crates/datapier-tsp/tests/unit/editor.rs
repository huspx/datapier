use super::*;

#[test]
fn utf16_selection_edit_and_undo_preserve_unicode() {
    let mut editor = Editor::new("中😀文");
    assert!(!editor.set_selection(1, 2));
    assert!(editor.set_selection(1, 3));
    assert_eq!(editor.selected_text(), Some("😀"));
    assert!(editor.key(Key::Text("好"), false));
    assert_eq!(editor.text, "中好文");
    assert!(editor.undo());
    assert_eq!(editor.selected_text(), Some("😀"));
    assert!(editor.key(Key::Backspace, false));
    assert_eq!(editor.text, "中文");
}

#[test]
fn shift_selection_tracks_utf16_across_chinese_and_emoji() {
    let mut editor = Editor::new("中😀文");
    assert!(editor.key(Key::Left, true));
    assert_eq!((editor.anchor, editor.cursor), (4, 3));
    assert_eq!(editor.selected_text(), Some("文"));
    assert!(editor.key(Key::Left, true));
    assert_eq!((editor.anchor, editor.cursor), (4, 1));
    assert_eq!(editor.selected_text(), Some("😀文"));
    assert!(editor.key(Key::Left, true));
    assert_eq!((editor.anchor, editor.cursor), (4, 0));
    assert_eq!(editor.selected_text(), Some("中😀文"));
    assert!(editor.key(Key::Right, true));
    assert_eq!((editor.anchor, editor.cursor), (4, 1));
    assert_eq!(editor.selected_text(), Some("😀文"));
    assert!(editor.key(Key::Right, false));
    assert_eq!((editor.anchor, editor.cursor), (4, 4));
    assert_eq!(editor.selected_text(), None);
}

#[test]
fn vertical_navigation_preserves_character_column_and_shift_anchor() {
    let mut editor = Editor::new("甲😀乙\na😀z\n短");
    assert!(editor.set_selection(3, 3));
    assert!(editor.key(Key::Down, true));
    assert_eq!(editor.cursor, 8);
    assert_eq!(editor.anchor, 3);
    assert_eq!(editor.selected_text(), Some("乙\na😀"));
    assert!(editor.key(Key::Down, false));
    assert_eq!(editor.cursor, 11);
    assert_eq!(editor.anchor, 11);
    assert!(editor.key(Key::Up, false));
    assert_eq!(editor.cursor, 6);
}

#[test]
fn mysql_statement_respects_quotes_comments_and_mode() {
    let sql = "SELECT '😀;x' /* ; */; SELECT `中;文`;";
    let cursor = sql[..sql.find("`中").unwrap()].encode_utf16().count();
    let range = current_statement(sql, cursor, MysqlMode::default()).unwrap();
    assert_eq!(
        &sql[utf16_to_byte(sql, range.start).unwrap()..utf16_to_byte(sql, range.end).unwrap()],
        "SELECT `中;文`"
    );
    assert!(current_statement("SELECT 'unterminated;", 0, MysqlMode::default()).is_err());
    assert!(
        current_statement(
            "CREATE DEFINER = root PROCEDURE p() BEGIN SELECT 1; END;",
            0,
            MysqlMode::default()
        )
        .is_err()
    );
    assert!(current_statement("SELECT 1; \\g", 0, MysqlMode::default()).is_err());
}

#[test]
fn mysql_rejects_ambiguous_comments_and_client_directives() {
    let mode = MysqlMode::default();
    for sql in [
        "SELECT 1 /*!80000 ; SELECT 2 */;",
        "SELECT 1 /*M!100000 ; SELECT 2 */;",
        "SELECT 1 /* outer /* inner */ ; SELECT 2 */;",
        "SELECT 1 # comment\r; SELECT 2",
        "DELIMITER $$\nSELECT 1$$",
        "SET @@session.`sql_mode` = ''; SELECT 1;",
    ] {
        assert!(current_statement(sql, 0, mode).is_err(), "{sql}");
    }
    let sql = "SELECT 1 -- comment;\r\n; SELECT 2";
    let cursor = sql.find("SELECT 2").unwrap();
    let range = current_statement(sql, cursor, mode).unwrap();
    assert_eq!(&sql[range], "SELECT 2");
}

#[test]
fn mysql_modes_control_backslash_and_double_quotes() {
    let string_sql = r"SELECT 'a\'; SELECT 2;";
    assert!(current_statement(string_sql, 0, MysqlMode::default()).is_err());
    let no_escape = MysqlMode {
        no_backslash_escapes: true,
        ansi_quotes: false,
    };
    assert_eq!(
        &string_sql[current_statement(string_sql, 0, no_escape).unwrap()],
        r"SELECT 'a\'"
    );

    let quoted_sql = r#"SELECT "a\"; SELECT 2;"#;
    assert!(current_statement(quoted_sql, 0, MysqlMode::default()).is_err());
    let ansi = MysqlMode {
        no_backslash_escapes: false,
        ansi_quotes: true,
    };
    assert_eq!(
        &quoted_sql[current_statement(quoted_sql, 0, ansi).unwrap()],
        r#"SELECT "a\""#
    );
}

#[test]
fn statement_gutter_ranges_follow_tokens_and_utf16() {
    let sql = "-- 说明;\nSELECT '😀;'; SELECT `x;y`\nFROM t";
    let items = statements(sql, MysqlMode::default()).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!((items[0].first_line, items[0].last_line), (2, 2));
    assert_eq!((items[1].first_line, items[1].last_line), (2, 3));
    let bytes = utf16_to_byte(sql, items[1].range.start).unwrap()
        ..utf16_to_byte(sql, items[1].range.end).unwrap();
    assert_eq!(&sql[bytes], "SELECT `x;y`\nFROM t");
    assert!(
        statements(
            "CREATE PROCEDURE p() BEGIN SELECT 1; END",
            MysqlMode::default()
        )
        .is_err()
    );
    assert!(statements("SELECT '未结束", MysqlMode::default()).is_err());
}

#[test]
fn explain_and_format_are_safe_for_selection_and_undo() {
    let mode = MysqlMode::default();
    assert_eq!(
        explain_selection_in("SELECT 1", SqlContext::Session(mode)).unwrap(),
        "EXPLAIN FORMAT=JSON SELECT 1"
    );
    assert!(explain_selection_in("SELECT 1; SELECT 2", SqlContext::Session(mode)).is_err());
    assert!(
        explain_selection_in(
            "DELETE FROM t",
            SqlContext::Session(MysqlMode {
                ansi_quotes: true,
                ..mode
            })
        )
        .is_err()
    );
    let mut editor = Editor::new("前;select '😀', ? from t;后");
    let start = "前;".encode_utf16().count();
    let end = "前;select '😀', ? from t".encode_utf16().count();
    editor.set_selection(start, end);
    let formatted = format_range(&editor.text, editor.selection(), mode).unwrap();
    assert!(formatted.contains("'😀'") && formatted.contains('?'));
    editor.edit(editor.selection(), &formatted);
    assert!(editor.text.starts_with("前;") && editor.text.ends_with(";后"));
    assert!(editor.undo());
    assert_eq!(editor.text, "前;select '😀', ? from t;后");
    assert!(format_selection("SELECT 1 /*! hint */", mode).is_err());
    assert!(format_selection("SELECT 1--2", mode).is_err());
}

#[test]
fn stateless_ranges_and_current_statement_use_utf16() {
    let sql = "SELECT '😀;中';\n SELECT `表` FROM t;";
    let context = SqlContext::Stateless;
    let items = statements_in(sql, context).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!((items[0].first_line, items[1].first_line), (1, 2));
    let cursor = sql[..sql.find("FROM").unwrap()].encode_utf16().count();
    let range = current_statement_in(sql, cursor, context).unwrap();
    assert_eq!(range, items[1].range);
    let start = utf16_to_byte(sql, range.start).unwrap();
    let end = utf16_to_byte(sql, range.end).unwrap();
    assert_eq!(&sql[start..end], "SELECT `表` FROM t");
    assert!(current_statement_in(sql, "SELECT '".len() + 1, context).is_err());
    assert!(current_statement_in(sql, sql.encode_utf16().count(), context).is_err());
}

#[test]
fn stateless_explain_and_format_reject_mode_dependent_input() {
    let context = SqlContext::Stateless;
    assert_eq!(
        explain_selection_in("SELECT '😀'", context).unwrap(),
        "EXPLAIN FORMAT=JSON SELECT '😀'"
    );
    assert!(explain_selection_in("SELECT 1; SELECT 2", context).is_err());
    for sql in [r#"SELECT "x""#, r"SELECT 'a\b'", "SELECT 1 /* comment */"] {
        assert!(statements_in(sql, context).is_err(), "{sql}");
        assert!(explain_selection_in(sql, context).is_err(), "{sql}");
        assert!(
            format_range_in(sql, 0..sql.encode_utf16().count(), context).is_err(),
            "{sql}"
        );
    }
    let mut editor = Editor::new("SELECT 1;select '😀' from t;SELECT 2");
    let start = "SELECT 1;".encode_utf16().count();
    let end = "SELECT 1;select '😀' from t".encode_utf16().count();
    assert!(editor.set_selection(end, start));
    let formatted = format_range_in(&editor.text, editor.selection(), context).unwrap();
    assert!(editor.edit(editor.selection(), &formatted));
    assert!(editor.undo());
    assert_eq!(editor.text, "SELECT 1;select '😀' from t;SELECT 2");
    assert_eq!((editor.anchor, editor.cursor), (end, start));
}
