use super::*;

#[test]
fn command_parser_keeps_binary_and_quoted_newlines() {
    let commands = parse_commands("SET \"a\\x00b\" \"line1\nline2\"\nGET 'a\\'b'").unwrap();
    assert_eq!(
        commands,
        vec![
            vec![b"SET".to_vec(), b"a\0b".to_vec(), b"line1\nline2".to_vec()],
            vec![b"GET".to_vec(), b"a'b".to_vec()]
        ]
    );
}

#[test]
fn read_only_commands_do_not_accept_scripts_or_transactions() {
    assert!(read_command(&args(&["GET", "key"])));
    assert!(!read_command(&args(&["EVAL", "return 1", "0"])));
    assert!(!read_command(&args(&["MULTI"])));
    assert!(!read_command(&args(&["SORT", "key", "STORE", "out"])));
}

#[test]
fn authentication_detection_respects_command_boundaries_and_escapes() {
    assert!(redis_contains_auth("PING\n'aU'\\x54H user secret").unwrap());
    assert!(redis_contains_auth("HELLO 3 AUTH user secret").unwrap());
    assert!(!redis_contains_auth("SET key 'AUTH secret'\nGET key").unwrap());
    assert!(!redis_contains_auth("HELLO 3 SETNAME authless").unwrap());
    assert!(redis_contains_auth("AUTH 'unfinished").is_err());
}

#[test]
fn analysis_tracks_selected_databases_and_rejects_unknown_scope() {
    let analysis =
        analyze_command("GET key\nSELECT 3\nSET key value\nMULTI\nDEL key\nEXEC").unwrap();
    assert_eq!(analysis["databases"], json!([3]));
    assert_eq!(analysis["change_database"], 3);
    assert_eq!(analysis["may_write"], true);
    assert_eq!(analysis["dangerous"], true);
    assert_eq!(analysis["commands"][1]["select_database"], 3);
    assert!(analyze_command("EVAL 'return 1' 0").is_err());
    assert!(analyze_command("COPY a b DB 2").is_err());
    assert!(analyze_command("SELECT invalid").is_err());
}
