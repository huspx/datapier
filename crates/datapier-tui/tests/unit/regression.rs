use super::{forms, reset_page_feedback};
use serde_json::json;

fn set(form: &mut forms::Form, key: &str, value: &str) {
    form.fields
        .iter_mut()
        .find(|field| field.key == key)
        .unwrap()
        .value = value.into();
}

#[test]
fn direct_mysql_options_roundtrip_and_validate() {
    let mut form = forms::direct(None, &json!("dangerous"));
    assert_eq!(form.get("mysql_timezone"), "+00:00");
    set(&mut form, "mysql_timezone", "");
    set(&mut form, "mysql_connect_timeout_seconds", "10");
    set(&mut form, "mysql_query_timeout_seconds", "86400");
    let profile = forms::direct_value(&form).unwrap();
    assert_eq!(
        profile["connection"]["mysql"],
        json!({
            "timezone": null,
            "connect_timeout_seconds": 10,
            "query_timeout_seconds": 86400
        })
    );
    let mut edited = forms::direct(Some(&profile), &json!("off"));
    assert_eq!(edited.get("mysql_timezone"), "");
    assert_eq!(edited.get("mysql_connect_timeout_seconds"), "10");
    set(&mut edited, "mysql_query_timeout_seconds", "86401");
    assert!(forms::direct_value(&edited).is_err());
    assert_eq!(edited.get("mysql_query_timeout_seconds"), "86401");
    set(&mut edited, "provider", "redis");
    assert!(forms::direct_value(&edited).is_ok());
}

#[test]
fn credential_updates_follow_current_form_and_require_matching_login() {
    let mut form = forms::server(None);
    set(&mut form, "password", "first");
    set(&mut form, "password_action", "keyring");
    set(&mut form, "totp", "otpauth://totp/test?secret=ABC");
    set(&mut form, "totp_action", "session");
    let login = forms::server_credential_updates(&form).unwrap();
    assert!(forms::credentials_match_login(&login, Some(&login)));
    assert!(!forms::credentials_match_login(&login, None));

    set(&mut form, "password_action", "clear");
    set(&mut form, "totp_action", "clear");
    let clear = forms::server_credential_updates(&form).unwrap();
    assert_eq!(
        clear,
        json!({"password_update":{"kind":"clear"},"totp_update":{"kind":"clear"}})
    );
    assert!(!forms::credentials_match_login(&clear, Some(&login)));
    assert!(forms::credentials_match_login(&clear, Some(&clear)));

    set(&mut form, "password_action", "session");
    set(&mut form, "password", "changed");
    let changed = forms::server_credential_updates(&form).unwrap();
    assert!(!forms::credentials_match_login(&changed, Some(&login)));
    set(&mut form, "password", "first");
    set(&mut form, "totp_action", "session");
    assert!(!forms::credentials_match_login(
        &forms::server_credential_updates(&form).unwrap(),
        Some(&login)
    ));
    set(&mut form, "password", "");
    assert!(forms::server_credential_updates(&form).is_err());
    set(&mut form, "password_action", "keep");
    set(&mut form, "totp", "");
    assert!(forms::server_credential_updates(&form).is_err());
}

#[test]
fn page_change_clears_previous_error_and_success_feedback() {
    let mut status = "invalid setting".to_string();
    let mut done = true;
    let mut field_error = Some(("limit".to_string(), "invalid".to_string()));
    reset_page_feedback(&mut status, &mut done, &mut field_error);
    assert!(status.is_empty());
    assert!(!done);
    assert!(field_error.is_none());
}

#[test]
fn long_field_tail_fits_terminal_cells() {
    let text = format!("path: {}中文/end", "a".repeat(100));
    let visible = forms::visible_tail(&text, 20);
    assert!(visible.starts_with('…'));
    assert!(visible.ends_with("中文/end"));
    assert!(ratatui::text::Span::raw(visible.as_str()).width() <= 20);
    assert_eq!(forms::visible_tail("abc", 0), "");
}

#[test]
fn archery_reselection_preserves_saved_identity_and_pending_edits() {
    let saved = json!({
        "id":"00000000-0000-0000-0000-000000000042", "revision":7,
        "name":"自定义连接名", "provider":"mysql", "access_mode":"read_only",
        "approval_policy":"off", "connection":{
            "kind":"archery", "server_id":"server-1", "instance_id":42,
            "instance_name":"remote", "databases":["a","b"], "default_database":"b"
        }
    });
    let instance = json!({"id":42,"name":"remote","provider":"mysql"});
    let baseline = vec![saved.clone()];
    let selected = vec!["b".to_owned()];
    // 从待选集合移除后重新确认，保留保存身份，只更新本次数据库选择。
    let reselected = forms::archery_selected_profile(
        &[],
        &baseline,
        &instance,
        &json!("server-1"),
        &json!("dangerous"),
        &selected,
        "b",
    );
    let mut expected = saved.clone();
    expected["connection"]["databases"] = json!(["b"]);
    assert_eq!(reselected, expected);

    let mut pending = saved;
    pending["name"] = json!("本轮修改的名称");
    pending["approval_policy"] = json!("dangerous");
    let mut expected = pending.clone();
    expected["connection"]["databases"] = json!(["b"]);
    assert_eq!(
        forms::archery_selected_profile(
            &[pending],
            &baseline,
            &instance,
            &json!("server-1"),
            &json!("off"),
            &selected,
            "b",
        ),
        expected
    );

    let fresh = forms::archery_selected_profile(
        &[],
        &baseline,
        &json!({"id":43,"name":"new","provider":"mysql"}),
        &json!("server-1"),
        &json!("dangerous"),
        &selected,
        "b",
    );
    assert_ne!(fresh["id"], baseline[0]["id"]);
    assert_eq!(fresh["revision"], 1);
    assert_eq!(fresh["approval_policy"], "dangerous");
}
