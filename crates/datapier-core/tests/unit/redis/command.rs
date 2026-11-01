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
