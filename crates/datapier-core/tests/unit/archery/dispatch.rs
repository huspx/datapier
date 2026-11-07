use super::*;
use crate::{Store, execution::ExecutionSink, providers::archery::ArcherySession};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use uuid::Uuid;

#[tokio::test]
async fn cancelling_query_waiting_for_service_permit_remains_unsent() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = ArcheryServer {
        id: Uuid::new_v4(),
        name: "fixture".into(),
        base_url: format!("http://{}/prefix/", listener.local_addr().unwrap()),
        username: "tester".into(),
        http_tls: HttpTls::default(),
        totp_mode: Default::default(),
        revision: 1,
    };
    let slot = Arc::new(AuthSlot::new(server.clone()).unwrap());
    let transport = slot.transport(&server.http_tls).unwrap();
    transport
        .jar
        .add_cookie_str(concat!("csrftoken", "=testcsrf; Path=/prefix/"), &slot.base);
    {
        let mut state = slot.state.lock().await;
        state.transport = Some(transport);
        state.status.state = "ready".into();
    }
    let arrived = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let count = Arc::new(AtomicUsize::new(0));
    let fixture = {
        let arrived = arrived.clone();
        let release = release.clone();
        let count = count.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let arrived = arrived.clone();
                let release = release.clone();
                let count = count.clone();
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut buf = [0; 4096];
                    loop {
                        let n = stream.read(&mut buf).await.unwrap();
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
                    assert!(String::from_utf8_lossy(&data).starts_with("POST /prefix/query/ "));
                    count.fetch_add(1, Ordering::AcqRel);
                    arrived.add_permits(1);
                    release.acquire().await.unwrap().forget();
                    let body = r#"{"status":0,"data":{"column_list":["v"],"column_type":["LONG"],"rows":[[1]],"affected_rows":1}}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                });
            }
        })
    };
    let root = std::env::temp_dir().join(format!("datapier-archery-dispatch-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "fixture".into(),
        revision: 1,
        approval_policy: Default::default(),
        provider: ProviderKind::Mysql,
        access_mode: AccessMode::ReadOnly,
        connection: ConnectionConfig::Archery(ArcheryConnection {
            server_id: server.id,
            instance_id: 7,
            instance_name: "one".into(),
            databases: vec!["db".into()],
            default_database: "db".into(),
        }),
    };
    let owner = Uuid::new_v4();
    let mut handles = Vec::new();
    let mut ids = Vec::new();
    let mut cancels = Vec::new();
    for _ in 0..5 {
        let task = store
            .create_task(owner, "scope", "execution.start", json!({}), false)
            .await
            .unwrap();
        ids.push(task.id);
        let cancel = CancellationToken::new();
        let sink = ExecutionSink::new(store.clone(), task.id)
            .with_dispatch_control(Arc::new(Mutex::new(())), cancel.clone());
        let mut session = ArcherySession {
            capabilities: profile.capabilities(),
            profile: profile.clone(),
            auth: slot.clone(),
            database: "db".into(),
            closed: false,
        };
        let task_cancel = cancel.clone();
        handles.push(tokio::spawn(async move {
            session
                .call(
                    "execution.run",
                    json!({"database":"db","text":"SELECT 1"}),
                    &sink,
                    &task_cancel,
                )
                .await
        }));
        cancels.push(cancel);
        if handles.len() == 4 {
            tokio::time::timeout(Duration::from_secs(3), arrived.acquire_many(4))
                .await
                .unwrap()
                .unwrap()
                .forget();
        }
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while slot.busy.load(Ordering::Acquire) != 5 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancels[4].cancel();
    assert_eq!(
        handles.pop().unwrap().await.unwrap().unwrap_err().code,
        "cancelled"
    );
    assert!(!store.task(ids[4]).await.unwrap().sent);
    assert_eq!(count.load(Ordering::Acquire), 4);
    release.add_permits(4);
    for handle in handles {
        handle.await.unwrap().unwrap();
    }
    fixture.abort();
    drop(store);
    let _ = tokio::fs::remove_dir_all(root).await;
}
