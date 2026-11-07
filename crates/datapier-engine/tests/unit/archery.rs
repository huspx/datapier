use super::*;
use datapier_core::model::{
    ArcheryConnection, ArcheryServer, ConnectionConfig, HttpTls, ProviderKind, TotpMode,
};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

#[path = "archery_boundaries.rs"]
mod boundaries;

#[path = "archery/auth.rs"]
mod auth;
#[path = "archery/capabilities.rs"]
mod capabilities;
#[path = "archery/configuration.rs"]
mod configuration;
#[path = "archery/execution.rs"]
mod execution;
#[path = "archery/limits_handoff.rs"]
mod limits_handoff;

struct Fixture {
    server: ArcheryServer,
    requests: Arc<Mutex<Vec<String>>>,
    block_resource: Arc<AtomicBool>,
    block_query: Arc<AtomicBool>,
    block_auth: Arc<AtomicBool>,
    block_verify: Arc<AtomicBool>,
    expire_query: Arc<AtomicBool>,
    challenge_auth: Arc<AtomicBool>,
    large_resource: Arc<AtomicBool>,
    large_ddl: Arc<AtomicBool>,
    query_count: Arc<AtomicUsize>,
    fail_query_at: Arc<AtomicUsize>,
    block_query_at: Arc<AtomicUsize>,
    arrived: Arc<Semaphore>,
    release: Arc<Semaphore>,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = ArcheryServer {
            id: Uuid::new_v4(),
            name: "fixture".into(),
            base_url: format!("http://{}/prefix/", listener.local_addr().unwrap()),
            username: "tester".into(),
            http_tls: HttpTls::default(),
            totp_mode: TotpMode::Manual,
            revision: 0,
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let block_resource = Arc::new(AtomicBool::new(false));
        let block_query = Arc::new(AtomicBool::new(false));
        let block_auth = Arc::new(AtomicBool::new(false));
        let block_verify = Arc::new(AtomicBool::new(false));
        let expire_query = Arc::new(AtomicBool::new(false));
        let challenge_auth = Arc::new(AtomicBool::new(false));
        let large_resource = Arc::new(AtomicBool::new(false));
        let large_ddl = Arc::new(AtomicBool::new(false));
        let query_count = Arc::new(AtomicUsize::new(0));
        let fail_query_at = Arc::new(AtomicUsize::new(0));
        let block_query_at = Arc::new(AtomicUsize::new(0));
        let auth_count = Arc::new(AtomicUsize::new(0));
        let arrived = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let task = {
            let requests = requests.clone();
            let block_resource = block_resource.clone();
            let block_query = block_query.clone();
            let block_auth = block_auth.clone();
            let block_verify = block_verify.clone();
            let expire_query = expire_query.clone();
            let challenge_auth = challenge_auth.clone();
            let large_resource = large_resource.clone();
            let large_ddl = large_ddl.clone();
            let query_count = query_count.clone();
            let fail_query_at = fail_query_at.clone();
            let block_query_at = block_query_at.clone();
            let auth_count = auth_count.clone();
            let arrived = arrived.clone();
            let release = release.clone();
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let requests = requests.clone();
                    let block_resource = block_resource.clone();
                    let block_query = block_query.clone();
                    let block_auth = block_auth.clone();
                    let block_verify = block_verify.clone();
                    let arrived = arrived.clone();
                    let expire_query = expire_query.clone();
                    let challenge_auth = challenge_auth.clone();
                    let large_resource = large_resource.clone();
                    let large_ddl = large_ddl.clone();
                    let query_count = query_count.clone();
                    let fail_query_at = fail_query_at.clone();
                    let block_query_at = block_query_at.clone();
                    let auth_count = auth_count.clone();
                    let release = release.clone();
                    tokio::spawn(async move {
                        let mut data = Vec::new();
                        let mut buf = [0; 4096];
                        loop {
                            let Ok(n) = stream.read(&mut buf).await else {
                                return;
                            };
                            if n == 0 {
                                return;
                            }
                            data.extend_from_slice(&buf[..n]);
                            if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                                let headers = String::from_utf8_lossy(&data[..end]);
                                let length = headers
                                    .lines()
                                    .find_map(|line| {
                                        line.to_ascii_lowercase()
                                            .strip_prefix("content-length:")
                                            .and_then(|n| n.trim().parse::<usize>().ok())
                                    })
                                    .unwrap_or(0);
                                if data.len() >= end + 4 + length {
                                    break;
                                }
                            }
                        }
                        let request = String::from_utf8_lossy(&data).into_owned();
                        let first = request.lines().next().unwrap().to_owned();
                        let bad_password = request.contains("password=wrong");
                        let full_columns = request.contains("SHOW+FULL+COLUMNS");
                        requests.lock().unwrap().push(request);
                        let resource = first.starts_with("GET /prefix/instance/instance_resource/");
                        let query = first.starts_with("POST /prefix/query/ ");
                        let query_number = if query {
                            query_count.fetch_add(1, Ordering::AcqRel) + 1
                        } else {
                            0
                        };
                        if (resource && block_resource.load(Ordering::Acquire))
                            || (query
                                && (block_query.load(Ordering::Acquire)
                                    || block_query_at.load(Ordering::Acquire) == query_number))
                            || (first.starts_with("POST /prefix/authenticate/ ")
                                && block_auth.load(Ordering::Acquire))
                            || (first.starts_with("POST /prefix/api/v1/user/2fa/verify/ ")
                                && block_verify.load(Ordering::Acquire))
                        {
                            arrived.add_permits(1);
                            let Ok(permit) = release.acquire().await else {
                                return;
                            };
                            permit.forget();
                        }
                        let cookie = if first.starts_with("POST /prefix/authenticate/ ") {
                            format!(
                                "Set-Cookie: sessionid=client{}; Path=/prefix/\r\n",
                                auth_count.fetch_add(1, Ordering::AcqRel) + 1
                            )
                        } else {
                            String::new()
                        };
                        let resource_body = if resource && large_resource.load(Ordering::Acquire) {
                            Some(
                                json!({"status":0,"data":["r".repeat(4 * 1024 * 1024)]})
                                    .to_string(),
                            )
                        } else {
                            None
                        };
                        let metadata_body = if query && large_ddl.load(Ordering::Acquire) {
                            let columns = if full_columns {
                                vec![
                                    "Field", "Type", "Null", "Key", "Default", "Extra", "Comment",
                                ]
                            } else {
                                vec![
                                    "Key_name",
                                    "Non_unique",
                                    "Seq_in_index",
                                    "Column_name",
                                    "Sub_part",
                                    "Index_type",
                                    "Collation",
                                ]
                            };
                            Some(json!({"status":0,"data":{"column_list":columns,"rows":[],"affected_rows":0}}).to_string())
                        } else {
                            None
                        };
                        let ddl_body = if first.starts_with("POST /prefix/instance/describetable/ ")
                        {
                            Some(json!({"status":0,"data":{"column_list":["Table","Create Table"],"rows":[["db_a",format!("CREATE TABLE db_a (id INT) /*{}*/", "d".repeat(4 * 1024 * 1024))]]}}).to_string())
                        } else {
                            None
                        };
                        let check_body = if first
                            .starts_with("POST /prefix/api/v1/workflow/sqlcheck/ ")
                        {
                            let rows: Vec<_> = (0..80).map(|id| json!({"id":id,"errlevel":1,"errormessage":"e".repeat(2048),"sql":format!("UPDATE t SET a={id}")})).collect();
                            Some(json!({"checked":"True","warning":null,"error":null,"warning_count":80,"error_count":0,"rows":rows,"status":null}).to_string())
                        } else {
                            None
                        };
                        let (extra, body) = if first.starts_with("GET /prefix/login/ ") {
                            (
                                concat!("Set-Cookie: csrftoken", "=testcsrf; Path=/prefix/\r\n"),
                                "login",
                            )
                        } else if first.starts_with("POST /prefix/authenticate/ ") && bad_password {
                            ("", r#"{"status":1,"data":null}"#)
                        } else if first.starts_with("POST /prefix/authenticate/ ")
                            && challenge_auth.load(Ordering::Acquire)
                        {
                            ("", r#"{"status":0,"data":"temporary"}"#)
                        } else if first.starts_with("POST /prefix/authenticate/ ") {
                            (cookie.as_str(), r#"{"status":0,"data":null}"#)
                        } else if first.starts_with("GET /prefix/login/2fa/ ") {
                            (
                                "",
                                "if ('verify_only' === 'verify_config') {} [{'code': 'totp'}]",
                            )
                        } else if first.starts_with("POST /prefix/api/v1/user/2fa/verify/ ") {
                            (cookie.as_str(), r#"{"status":0}"#)
                        } else if first.starts_with("GET /prefix/group/user_all_instances/") {
                            (
                                "",
                                r#"{"status":0,"data":[{"id":7,"instance_name":"one","db_type":"mysql"},{"id":8,"instance_name":"two","db_type":"mysql"}]}"#,
                            )
                        } else if resource {
                            (
                                "",
                                resource_body
                                    .as_deref()
                                    .unwrap_or(r#"{"status":0,"data":["db_a","db_b"]}"#),
                            )
                        } else if query && fail_query_at.load(Ordering::Acquire) == query_number {
                            ("", r#"{"status":1,"msg":"query denied","data":null}"#)
                        } else if let Some(body) = metadata_body.as_deref().or(ddl_body.as_deref())
                        {
                            ("", body)
                        } else if query {
                            (
                                "",
                                r#"{"status":0,"data":{"column_list":["v"],"column_type":["LONG"],"rows":[[1]],"affected_rows":1}}"#,
                            )
                        } else if let Some(body) = check_body.as_deref() {
                            ("", body)
                        } else if first.starts_with("GET /prefix/query/querylog/") {
                            ("", r#"{"detail":"history denied"}"#)
                        } else {
                            panic!("unexpected request: {first}")
                        };
                        let status = if query && expire_query.load(Ordering::Acquire) {
                            "401 Unauthorized"
                        } else if first.starts_with("GET /prefix/query/querylog/") {
                            "403 Forbidden"
                        } else {
                            "200 OK"
                        };
                        let response = format!(
                            "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            })
        };
        Self {
            server,
            requests,
            block_resource,
            block_query,
            block_auth,
            block_verify,
            expire_query,
            challenge_auth,
            large_resource,
            large_ddl,
            query_count,
            fail_query_at,
            block_query_at,
            arrived,
            release,
            task,
        }
    }
    async fn wait(&self) {
        tokio::time::timeout(Duration::from_secs(3), self.arrived.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn profile(server_id: Uuid, instance_id: i64, name: &str) -> Profile {
    Profile {
        id: Uuid::new_v4(),
        name: name.into(),
        revision: 1,
        approval_policy: Default::default(),
        provider: ProviderKind::Mysql,
        access_mode: AccessMode::ReadOnly,
        connection: ConnectionConfig::Archery(ArcheryConnection {
            server_id,
            instance_id,
            instance_name: name.into(),
            databases: vec!["db_a".into(), "db_b".into()],
            default_database: "db_a".into(),
        }),
    }
}

async fn login(engine: &Arc<Engine>, owner: Uuid, fixture: &Fixture) -> Value {
    engine
        .dispatch(
            owner,
            "archery.auth.begin",
            json!({"server":fixture.server,"expected_revision":fixture.server.revision,"password":"good","operation_id":Uuid::new_v4()}),
        )
        .await
        .unwrap()
}

async fn setup() -> (Arc<Engine>, PathBuf, Uuid, Fixture, Value) {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    let fixture = Fixture::new().await;
    let auth = login(&engine, owner, &fixture).await;
    assert_eq!(auth["state"], "ready");
    engine.dispatch(owner, "archery.server.save", json!({"auth_id":auth["auth_id"],"server":fixture.server,"expected_revision":0,"password_update":{"kind":"replace","value":"good","persist":false}})).await.unwrap();
    let auth = engine
        .dispatch(
            owner,
            "archery.auth.status",
            json!({"auth_id":auth["auth_id"]}),
        )
        .await
        .unwrap();
    (engine, root, owner, fixture, auth)
}

async fn save(
    engine: &Arc<Engine>,
    owner: Uuid,
    fixture: &Fixture,
    auth: &Value,
    profiles: Vec<Profile>,
) -> Result<Value> {
    let expected: Vec<_> = engine
        .store
        .profiles()
        .await?
        .into_iter()
        .filter(|p| {
            p.archery()
                .is_some_and(|a| a.server_id == fixture.server.id)
        })
        .collect();
    engine.dispatch(owner, "archery.profile.apply_batch", json!({"auth_id":auth["auth_id"],"server_id":fixture.server.id,"expected_server_revision":1,"expected_profiles":expected,"desired_profiles":profiles})).await
}

async fn terminal_task(engine: &Arc<Engine>, owner: Uuid, id: &Value) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let task = engine
                .dispatch(owner, "execution.get", json!({"task_id":id}))
                .await
                .unwrap();
            if !matches!(
                task["state"].as_str(),
                Some("queued" | "running" | "cancelling")
            ) {
                return task;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
