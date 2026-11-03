use super::*;
use serde_json::Value;

#[test]
fn selection_uses_raw_identity_and_prunes_invisible_keys() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("selection".into());
    app.profile_form(None);
    app.field_set("provider", "redis");
    app.profile = Some(app.form_profile().unwrap());
    app.workbench.overlay = None;
    app.resources = ["", "/w==", "YmFzZTY0Og=="]
        .into_iter()
        .map(|key| crate::Resource {
            name: "相同展示名".into(),
            kind: "string".into(),
            database: Some("0".into()),
            key_b64: Some(key.into()),
            data: Value::Null,
        })
        .collect();
    let old_event = format!("{}:{}:1:/w==", app.tabs[app.active].id, app.generation);
    assert_eq!(app.redis_key_action_index(&old_event), Some(1));
    app.redis_key_click(0, false, true, &backend);
    app.redis_key_click(2, true, false, &backend);
    assert_eq!(app.selected_redis_key_count(), 3);
    let copy = app.redis_key_menu(1, "copy", &backend).unwrap();
    let names: Value = serde_json::from_str(&copy).unwrap();
    assert_eq!(names[0], "");
    assert_eq!(names[1]["base64"], "/w==");
    assert_eq!(names[2], "base64:");
    app.resources.remove(1);
    assert_eq!(app.redis_key_action_index(&old_event), None);
    app.redis_prune_key_selection();
    assert_eq!(app.selected_redis_key_count(), 2);
    app.profile.as_mut().unwrap().direct_mut().unwrap().redis_db = 1;
    app.redis_prune_key_selection();
    assert_eq!(app.selected_redis_key_count(), 0);
}
