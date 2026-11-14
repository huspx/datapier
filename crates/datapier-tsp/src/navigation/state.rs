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

#[derive(Clone, Default)]
pub(crate) struct RedisDiscovery {
    pub databases: Vec<String>,
    pub loading: bool,
    pub loaded: bool,
    pub error: Option<String>,
}

#[derive(Clone)]
pub(crate) struct ProfileDeleteConfirmation {
    pub token: Uuid,
    pub id: Uuid,
    pub name: String,
    pub busy: bool,
    pub error: Option<String>,
    pub(super) tab: Option<Uuid>,
    pub(super) profile: Value,
}

#[derive(Clone)]
pub(super) struct ProfileDeleteOperation {
    pub(super) id: Uuid,
    pub(super) token: Uuid,
}

#[derive(Clone)]
pub(crate) struct DatabaseCreate {
    pub token: Uuid,
    pub tab: Uuid,
    pub profile_id: Uuid,
    pub profile: Value,
    pub session: Option<Value>,
    pub options: Option<Value>,
    pub charset: String,
    pub collation: String,
    pub open_select: Option<String>,
    pub plan: Option<Value>,
    pub observed: (String, String, String),
    pub attempted: Option<(String, String, String)>,
    pub preview_input: Option<(String, String, String)>,
    pub plan_input: Option<(String, String, String)>,
    pub changed_at: Instant,
    pub sql: Option<String>,
    pub error: Option<String>,
    pub busy: bool,
    pub task: Option<String>,
    pub polling: bool,
    pub last_poll: Instant,
    pub unknown: bool,
    pub after_close: Option<String>,
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
    CreateDatabase {
        token: Uuid,
        stage: &'static str,
    },
    Tree {
        profile: Uuid,
        database: Option<String>,
        session: Value,
    },
    Form {
        profile: Value,
        secret: Value,
    },
    RedisDiscovery {
        profile_id: Option<Uuid>,
        profile: Option<Value>,
        secret: Value,
        session: Option<Value>,
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
    pub form_approval_policy: datapier_core::model::ApprovalPolicy,
    pub redis_db_menu: bool,
    pub form_redis_db_menu: bool,
    pub redis_discovery: BTreeMap<Uuid, RedisDiscovery>,
    pub form_redis_discovery: RedisDiscovery,
    pub(super) delete_confirmation: Option<ProfileDeleteConfirmation>,
    pub(super) delete_operation: Option<ProfileDeleteOperation>,
    pub(super) create_database: Option<DatabaseCreate>,
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
            form_approval_policy: datapier_core::model::ApprovalPolicy::default(),
            redis_db_menu: false,
            form_redis_db_menu: false,
            redis_discovery: BTreeMap::new(),
            form_redis_discovery: RedisDiscovery::default(),
            delete_confirmation: None,
            delete_operation: None,
            create_database: None,
            drop_confirmation: None,
            drop_operation: None,
            queued_sections: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }
}
