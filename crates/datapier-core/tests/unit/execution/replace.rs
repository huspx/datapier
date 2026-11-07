use super::*;
use crate::model::replace::{ReplaceScope, ReplaceSpec};

fn spec(pattern: &str, replacement: &str, regex: bool) -> ReplaceSpec {
    ReplaceSpec {
        pattern: pattern.into(),
        replacement: replacement.into(),
        regex,
        case_sensitive: true,
        multi_line: false,
        dot_matches_new_line: false,
        scope: ReplaceScope::Selected,
        columns: vec![],
        roles: vec![],
    }
}

#[test]
fn unicode_literal_and_literal_dollar() {
    let result = CompiledReplacement::new(&spec("猫", "$1\\", false))
        .unwrap()
        .apply("猫🙂猫", 100)
        .unwrap();
    assert_eq!(result.value, "$1\\🙂$1\\");
    assert_eq!(result.matches, 2);
}

#[test]
fn captures_and_invalid_references() {
    let engine =
        CompiledReplacement::new(&spec("(?P<name>猫)(🙂)", "${name}-$2-$$", true)).unwrap();
    assert_eq!(engine.apply("猫🙂", 100).unwrap().value, "猫-🙂-$");
    assert!(CompiledReplacement::new(&spec("(x)", "$2", true)).is_err());
    assert!(CompiledReplacement::new(&spec("x", "${missing}", true)).is_err());
}

#[test]
fn zero_width_and_output_limit() {
    let engine = CompiledReplacement::new(&spec("", "!", true)).unwrap();
    assert_eq!(engine.apply("猫🙂", 100).unwrap().value, "!猫!🙂!");
    assert!(engine.apply("猫🙂", 9).is_err());
    assert!(CompiledReplacement::new(&spec("", "x", false)).is_err());
}

#[test]
fn multiline_and_case_options() {
    let mut options = spec("^猫.$", "ok", true);
    options.multi_line = true;
    options.case_sensitive = false;
    assert_eq!(
        CompiledReplacement::new(&options)
            .unwrap()
            .apply("a\n猫X\nb", 100)
            .unwrap()
            .value,
        "a\nok\nb"
    );
}

#[test]
fn dot_newline_and_invalid_regex() {
    let mut options = spec("a.b", "x", true);
    assert!(
        !CompiledReplacement::new(&options)
            .unwrap()
            .apply("a\nb", 20)
            .unwrap()
            .changed
    );
    options.dot_matches_new_line = true;
    assert_eq!(
        CompiledReplacement::new(&options)
            .unwrap()
            .apply("a\nb", 20)
            .unwrap()
            .value,
        "x"
    );
    assert!(CompiledReplacement::new(&spec("(?=x)", "y", true)).is_err());
    assert!(CompiledReplacement::new(&spec("(", "y", true)).is_err());
}
