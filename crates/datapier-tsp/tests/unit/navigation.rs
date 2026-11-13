use super::*;
use datapier_core::model::RowChange;

#[path = "navigation/archery.rs"]
mod archery;
#[path = "navigation/create.rs"]
mod create;
#[path = "navigation/drop.rs"]
mod drop;
#[path = "navigation/profile.rs"]
mod profile;
#[path = "navigation/redis.rs"]
mod redis;
#[path = "navigation/tree.rs"]
mod tree;

fn redis_app() -> (App, Profile, Uuid) {
    let mut app = App::new("test".into());
    app.profile_form(None);
    app.field_set("name", "redis");
    app.field_set("provider", "redis");
    let profile = app.form_profile().unwrap();
    app.workbench.overlay = None;
    let session = Uuid::new_v4();
    app.profiles.push(profile.clone());
    app.profile = Some(profile.clone());
    app.tabs[0].profile_id = Some(profile.id);
    app.session = Some(json!({"session_id": session, "profile": profile}));
    app.dirty = false;
    (app, profile, session)
}

fn confirm_action(token: Uuid) -> String {
    format!("{}{}:{}", "profile-delete-", "confirm", token)
}

fn mysql_drop_app() -> (App, Profile) {
    let (mut app, mut profile, _) = redis_app();
    profile.provider = datapier_core::model::ProviderKind::Mysql;
    profile.access_mode = datapier_core::model::AccessMode::ReadWrite;
    app.profile = Some(profile.clone());
    app.profiles[0] = profile.clone();
    app.session.as_mut().unwrap()["profile"] = json!(profile);
    let tree = app.navigation.databases.entry(profile.id).or_default();
    tree.databases =
        vec![serde_json::from_value(json!({"name":"db`;x","kind":"database"})).unwrap()];
    tree.nodes.entry("db`;x".into()).or_default().resources =
        vec![serde_json::from_value(json!({"name":"t`; DROP TABLE y","kind":"table"})).unwrap()];
    (app, profile)
}

fn archery_app(provider: &str, databases: &[&str]) -> (App, Profile, Uuid) {
    let (mut app, _, session) = redis_app();
    let profile: Profile = serde_json::from_value(json!({
        "id": Uuid::new_v4(), "name": "platform", "provider": provider,
        "access_mode": "read_only",
        "connection": {
            "kind": "archery", "server_id": Uuid::new_v4(), "instance_id": 42,
            "instance_name": "instance", "databases": databases,
            "default_database": databases[0]
        }
    }))
    .unwrap();
    app.profiles = vec![profile.clone()];
    app.profile = Some(profile.clone());
    app.tabs[0].profile_id = Some(profile.id);
    app.session = Some(json!({"session_id":session,"profile":profile}));
    (app, profile, session)
}

fn tree_has_text(node: &Node<Msg>, text: &str) -> bool {
    node.props().get("text").and_then(Value::as_str) == Some(text)
        || node
            .child_nodes()
            .iter()
            .any(|child| tree_has_text(child, text))
}
