//! 连接来源与数据库图标。
use super::*;
pub(super) fn provider_icon(app: &App, redis: bool) -> Node<Msg> {
    if let Some(icons) = &app.icon_blobs {
        html::span("")
            .class("provider-icon")
            .child(
                ui::image()
                    .blob(if redis {
                        icons.redis.clone()
                    } else {
                        icons.mysql.clone()
                    })
                    .alt(if redis { "Redis" } else { "MySQL" })
                    .w(22.0)
                    .h(22.0),
            )
            .into()
    } else {
        html::span("")
            .class("provider-icon")
            .child(ui::icon("database"))
            .into()
    }
}

pub(super) fn archery_icon(app: &App) -> Node<Msg> {
    let mut icon = html::span("")
        .class("provider-icon")
        .attr("aria-label", "Archery");
    if let Some(icons) = &app.icon_blobs {
        icon = icon.child(
            ui::image()
                .blob(icons.archery.clone())
                .alt("Archery")
                .w(22.0)
                .h(22.0),
        );
    }
    icon.into()
}

pub(super) fn connection_provider_icon(app: &App, profile: &Profile) -> Node<Msg> {
    let mut icon = html::span("")
        .class("provider-source-icon")
        .child(provider_icon(
            app,
            profile.provider == datapier_core::model::ProviderKind::Redis,
        ));
    if profile.is_archery() {
        let label = if profile.provider == datapier_core::model::ProviderKind::Redis {
            "Archery · Redis"
        } else {
            "Archery · MySQL"
        };
        icon = icon.prop("title", label).attr("aria-label", label);
        if let Some(icons) = &app.icon_blobs {
            icon = icon.child(
                html::span("").class("archery-source-badge").child(
                    ui::image()
                        .blob(icons.archery_badge.clone())
                        .alt("Archery")
                        .w(11.0)
                        .h(11.0),
                ),
            );
        }
    }
    icon.into()
}
