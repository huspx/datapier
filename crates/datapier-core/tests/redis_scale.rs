//! 需显式启用的隔离 Redis 6.2 规模验收；不会清空数据库。
use datapier_core::{ExecutionSink, Store, model::*, providers::ProviderSession};
use redis::Client;
use serde_json::json;
use std::{
    error::Error,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

type TestResult<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
const KEYS: usize = 1_000_000;
const BATCH: usize = 1_000;

#[tokio::test]
#[ignore = "requires isolated Redis and explicit DATAPIER_TEST_REDIS_PORT and STRESS_KEYS=1000000"]
async fn million_keys_resource_list() -> TestResult<()> {
    let port: u16 = std::env::var("DATAPIER_TEST_REDIS_PORT")?.parse()?;
    if std::env::var("STRESS_KEYS")? != "1000000" {
        return Err("STRESS_KEYS must be exactly 1000000".into());
    }
    let prefix = format!("datapier-scale:{}:", Uuid::new_v4());
    let root = std::env::temp_dir().join(format!("datapier-scale-{}", Uuid::new_v4()));
    let client = Client::open(format!("redis://127.0.0.1:{port}/15"))?;
    let mut raw = client.get_multiplexed_async_connection().await?;
    let start = Instant::now();
    let result: TestResult<()> = async {
        let store = Store::open(&root).await?;
        let profile = Profile { id: Uuid::new_v4(), name: "redis-scale".into(), provider: ProviderKind::Redis,
        access_mode: AccessMode::ReadWrite, connection: ConnectionConfig::Direct(DirectConnection {
        host: "127.0.0.1".into(), port, username: String::new(), database: None,
        redis_db: 15, tls: TlsConfig::default(), visible_databases: None }) };
        let mut session = ProviderSession::connect(profile, None).await?;
        let task = store.create_task(Uuid::new_v4(), "redis-scale", "execution.start", json!({}), false).await?;
        let sink = ExecutionSink::new(store.clone(), task.id);
        for first in (0..KEYS).step_by(BATCH) {
            let mut pipe = redis::pipe();
            for index in first..(first + BATCH).min(KEYS) {
                pipe.cmd("SET").arg(format!("{prefix}{index:07}")).arg("v").ignore();
            }
            pipe.query_async::<()>(&mut raw).await?;
        }
        eprintln!("created={KEYS} elapsed={:.2}s", start.elapsed().as_secs_f64());
        let scan_start = Instant::now();
        let deadline = scan_start + Duration::from_secs(180);
        let cancel = CancellationToken::new();
        let mut seen = vec![0u8; KEYS.div_ceil(8)];
        let mut total = 0usize;
        let mut pages = 0usize;
        let mut cursor: Option<String> = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let response = tokio::time::timeout(remaining.min(Duration::from_secs(30)), session.call(
                "resource.list", json!({"pattern":format!("{prefix}*"),"limit":1000,"cursor":cursor}), &sink, &cancel,
            )).await.map_err(|_| format!("scan timed out after {:.2}s: keys={total}, pages={pages}", scan_start.elapsed().as_secs_f64()))??;
            let resources = response["resources"].as_array().ok_or("missing resources")?;
            if resources.len() > BATCH || resources.is_empty() && !response["complete"].as_bool().ok_or("missing complete")? {
                return Err(format!("invalid page size {} on page {}", resources.len(), pages + 1).into());
            }
            for resource in resources {
                if resource["kind"].as_str() != Some("string") { return Err("unexpected resource type".into()); }
                let name = resource["name"].as_str().ok_or("missing resource name")?;
                let suffix = name.strip_prefix(&prefix).ok_or("unexpected key prefix")?;
                if suffix.len() != 7 || !suffix.bytes().all(|b| b.is_ascii_digit()) { return Err("invalid key suffix".into()); }
                let index: usize = suffix.parse()?;
                if index >= KEYS { return Err("key index out of range".into()); }
                let mask = 1 << (index % 8);
                if seen[index / 8] & mask != 0 { return Err(format!("duplicate key {name}").into()); }
                seen[index / 8] |= mask;
                total += 1;
            }
            pages += 1;
            if pages.is_multiple_of(100) { eprintln!("scan keys={total} pages={pages} elapsed={:.2}s", scan_start.elapsed().as_secs_f64()); }
            let complete = response["complete"].as_bool().ok_or("missing complete")?;
            let next = response["cursor"].as_str().map(str::to_owned);
            if complete {
                if next.is_some() { return Err("complete page has cursor".into()); }
                break;
            }
            let next = next.ok_or("incomplete page missing cursor")?;
            if let Some(previous) = &cursor && previous != &next {
                return Err("scan token changed unexpectedly".into());
            }
            cursor = Some(next);
        }
        if total != KEYS { return Err(format!("missing keys: received {total} of {KEYS}").into()); }
        eprintln!("complete keys={total} pages={pages} scan_elapsed={:.2}s total_elapsed={:.2}s bitmap_bytes={}", scan_start.elapsed().as_secs_f64(), start.elapsed().as_secs_f64(), seen.len());
        session.close().await?;
        Ok(())
    }.await;
    // 即使准备或扫描失败也能确定本次命名空间；清理只覆盖测试自行生成的 key。
    let cleanup: TestResult<()> = async {
        for first in (0..KEYS).step_by(BATCH) {
            let mut pipe = redis::pipe();
            pipe.cmd("UNLINK");
            for index in first..(first + BATCH).min(KEYS) {
                pipe.arg(format!("{prefix}{index:07}"));
            }
            pipe.ignore();
            pipe.query_async::<()>(&mut raw).await?;
        }
        Ok(())
    }
    .await;
    let remove = if root.exists() {
        std::fs::remove_dir_all(&root)
    } else {
        Ok(())
    };
    if let Err(error) = &cleanup {
        eprintln!("cleanup failed for prefix {prefix}: {error}");
    }
    if let Err(error) = &remove {
        eprintln!(
            "temporary directory cleanup failed at {}: {error}",
            root.display()
        );
    }
    result?;
    cleanup?;
    remove?;
    Ok(())
}
