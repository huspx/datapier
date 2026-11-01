use super::*;

#[test]
fn language_saves_are_serialized_and_coalesce_to_latest_selection() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.application_action("language:en", &backend);
    let first = requests.try_recv().unwrap();
    app.application_action("language:zh-CN", &backend);
    app.application_action("language:system", &backend);
    assert!(requests.try_recv().is_err());
    app.settings_response(
        Response {
            tag: first.tag,
            result: Ok(first.params["settings"].clone()),
        },
        &backend,
    );
    let latest = requests.try_recv().unwrap();
    assert_eq!(latest.method, "settings.set");
    assert_eq!(latest.params["settings"]["language"], "system");
    assert_eq!(app.settings.language, LanguagePreference::System);
    app.settings_response(
        Response {
            tag: latest.tag,
            result: Ok(latest.params["settings"].clone()),
        },
        &backend,
    );
    assert!(!app.settings_unsaved);
    assert!(app.settings_save_token.is_none());
    assert!(requests.try_recv().is_err());
}

#[test]
fn failed_write_reports_error_and_still_saves_a_newer_selection() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.application_action("language:en", &backend);
    let first = requests.try_recv().unwrap();
    app.application_action("language:zh-CN", &backend);
    app.settings_response(
        Response {
            tag: first.tag,
            result: Err(AppError::new("test", "save failed")),
        },
        &backend,
    );
    let latest = requests.try_recv().unwrap();
    assert_eq!(latest.params["settings"]["language"], "zh-CN");
    assert!(format!("{:?}", app.status).contains("save failed"));
    app.settings_response(
        Response {
            tag: latest.tag,
            result: Err(AppError::new("test", "latest failed")),
        },
        &backend,
    );
    assert!(app.settings_unsaved);
    assert!(app.settings_save_token.is_none());
    assert_eq!(app.settings.language, LanguagePreference::ZhCn);
    assert!(format!("{:?}", app.status).contains("latest failed"));
    assert!(requests.try_recv().is_err());
}
