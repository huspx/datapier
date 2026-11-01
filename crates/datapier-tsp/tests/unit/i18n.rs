use super::*;

#[test]
fn locale_matches_script_before_region_and_uses_only_the_first_language() {
    for (locale, expected) in [
        ("zh", Language::ZhCn),
        ("zh_CN.UTF-8", Language::ZhCn),
        ("zh_SG@variant", Language::ZhCn),
        ("zh-Hans-TW", Language::ZhCn),
        ("zh-Hant-CN", Language::En),
        ("zh-TW", Language::En),
        ("zh-HK", Language::En),
        ("zh-MO", Language::En),
        ("en-CN", Language::En),
        ("C", Language::En),
        ("POSIX", Language::En),
        ("", Language::En),
        ("fr,zh-CN", Language::En),
        ("zh-XX", Language::En),
    ] {
        assert_eq!(normalize_locale(locale), expected, "{locale}");
    }
}
