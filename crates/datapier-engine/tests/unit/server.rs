use super::*;
use crate::protocol;

async fn engine() -> (Arc<Engine>, PathBuf) {
    let root = std::env::temp_dir().join(format!("datapier-engine-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let store = Store::open(&root).await.unwrap();
    let state = State {
        accepting: true,
        clients: HashSet::new(),
        contexts: HashMap::new(),
        windows: HashMap::new(),
        mcp: Default::default(),
        mcp_owners: HashSet::new(),
        client_cancels: HashMap::new(),
        event_senders: HashMap::new(),
        event_seq: 0,
        idle: Instant::now() - Duration::from_secs(10),
        databases: HashMap::new(),
        tasks: HashMap::new(),
        result_sessions: HashMap::new(),
        discovery: HashMap::new(),
        unpersisted: HashMap::new(),
        profile_locks: HashMap::new(),
        archery: Default::default(),
    };
    (
        Arc::new(Engine {
            id: Uuid::new_v4(),
            store,
            state: Mutex::new(state),
        }),
        root,
    )
}

#[path = "server/lifecycle.rs"]
mod lifecycle;
#[path = "server/redis.rs"]
mod redis;
#[path = "server/results.rs"]
mod results;
#[path = "server/sessions.rs"]
mod sessions;

#[path = "archery.rs"]
mod archery_tests;
