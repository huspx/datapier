use super::column_flags;

#[test]
fn auto_increment_and_generated_are_independent() {
    assert_eq!(column_flags("auto_increment", None), (true, false));
    assert_eq!(
        column_flags("VIRTUAL GENERATED", Some("`a` + 1")),
        (false, true)
    );
    assert_eq!(
        column_flags("DEFAULT_GENERATED on update CURRENT_TIMESTAMP", None),
        (false, false)
    );
    assert_eq!(
        column_flags("auto_increment DEFAULT_GENERATED", None),
        (true, false)
    );
}
