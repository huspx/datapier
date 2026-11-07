use super::*;
use crate::ExecutionSink;
use tokio_util::sync::CancellationToken;

#[path = "storage/configuration.rs"]
mod configuration;
#[path = "storage/credentials.rs"]
mod credentials;
#[path = "storage/results.rs"]
mod results;
#[path = "storage/tasks.rs"]
mod tasks;

async fn fixture() -> Store {
    let root = std::env::temp_dir().join(format!("datapier-store-{}", Uuid::new_v4()));
    Store::open(&root).await.expect("打开隔离测试目录")
}
async fn cleanup(store: Store) {
    let root = (*store.root).clone();
    store.pool.close().await;
    tokio::fs::remove_dir_all(root).await.expect("清理测试目录");
}
