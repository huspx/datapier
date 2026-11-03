use super::*;

pub(crate) fn navigation_visible_databases(profile: &Profile) -> Option<&Vec<String>> {
    match &profile.connection {
        datapier_core::model::ConnectionConfig::Direct(direct) => direct.visible_databases.as_ref(),
        datapier_core::model::ConnectionConfig::Archery(archery) => Some(&archery.databases),
    }
}

#[derive(Clone, Default)]
pub(crate) struct DatabaseNode {
    pub expanded: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub resources: Vec<Resource>,
    pub collapsed_kinds: BTreeSet<String>,
    pub(super) loaded: bool,
}

#[derive(Clone, Default)]
pub(crate) struct DatabaseTree {
    pub databases: Vec<Resource>,
    pub loading: bool,
    pub error: Option<String>,
    pub nodes: BTreeMap<String, DatabaseNode>,
    pub(super) loaded: bool,
}

#[derive(Clone, Default)]
pub(crate) struct DatabaseForm {
    pub loaded: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub candidates: Vec<String>,
    pub selected: BTreeSet<String>,
    pub all: bool,
}

#[derive(Clone)]
pub(crate) struct ProfileDeleteConfirmation {
    pub token: Uuid,
    pub id: Uuid,
    pub name: String,
    pub busy: bool,
    pub error: Option<String>,
    pub(super) tab: Uuid,
    pub(super) profile: Value,
}

#[derive(Clone)]
pub(super) struct ProfileDeleteOperation {
    pub(super) id: Uuid,
    pub(super) token: Uuid,
    pub(super) sessions: Vec<(Uuid, Value)>,
}

#[derive(Clone)]
pub(crate) struct ResourceDropConfirmation {
    pub token: Uuid,
    pub name: String,
    pub database: String,
    pub kind: &'static str,
    pub busy: bool,
    pub task_started: bool,
    pub cancel_requested: bool,
    pub error: Option<String>,
    pub(super) tab: Uuid,
    pub(super) generation: u64,
    pub(super) profile_id: Uuid,
    pub(super) profile: Value,
    pub(super) session_profile: Value,
    pub(super) source_profile: Value,
    pub(super) session: Value,
}

#[derive(Clone)]
pub(super) struct DropOperation {
    pub(super) confirmation: ResourceDropConfirmation,
    pub(super) task: Option<String>,
    pub(super) polling: bool,
    pub(super) last_poll: Instant,
    pub(super) ddl_session: Option<Value>,
    pub(super) sql: String,
    pub(super) terminal: Option<(String, String)>,
    pub(super) closing: bool,
    pub(super) cancel_requested: bool,
    pub(super) cancel_pending: bool,
}

#[derive(Clone)]
pub(super) struct QueuedSection {
    pub(super) target: ResourceTarget,
    pub(super) section: String,
}

#[derive(Clone)]
pub(super) enum Pending {
    Tree {
        profile: Uuid,
        database: Option<String>,
        session: Value,
    },
    Form {
        profile: Value,
        secret: Value,
    },
    Redis {
        tab: Uuid,
        generation: u64,
        db: u32,
    },
    DeleteProfile {
        id: Uuid,
        token: Uuid,
    },
    CloseProfileSession {
        token: Uuid,
        tab: Uuid,
        session: Value,
    },
    DropOpen {
        token: Uuid,
    },
    DropClose {
        token: Uuid,
    },
    DropCancel {
        token: Uuid,
    },
    DropStart {
        token: Uuid,
    },
    DropPoll {
        token: Uuid,
    },
}

/// 跨 tab 的导航状态；仅收展状态写入工作区，异步请求按 token 隔离。
#[derive(Clone)]
pub(crate) struct NavigationState {
    pub expanded: bool,
    pub narrow_expanded: bool,
    pub databases: BTreeMap<Uuid, DatabaseTree>,
    pub form: DatabaseForm,
    pub redis_db_menu: bool,
    pub form_redis_db_menu: bool,
    pub(super) delete_confirmation: Option<ProfileDeleteConfirmation>,
    pub(super) delete_operation: Option<ProfileDeleteOperation>,
    pub(super) drop_confirmation: Option<ResourceDropConfirmation>,
    pub(super) drop_operation: Option<DropOperation>,
    pub(super) queued_sections: BTreeMap<Uuid, QueuedSection>,
    pub(super) pending: BTreeMap<Uuid, Pending>,
}
impl Default for NavigationState {
    fn default() -> Self {
        Self {
            expanded: true,
            narrow_expanded: false,
            databases: BTreeMap::new(),
            form: DatabaseForm::default(),
            redis_db_menu: false,
            form_redis_db_menu: false,
            delete_confirmation: None,
            delete_operation: None,
            drop_confirmation: None,
            drop_operation: None,
            queued_sections: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }
}
