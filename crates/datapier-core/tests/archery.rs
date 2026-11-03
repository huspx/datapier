use datapier_core::{
    model::{ArcheryInstance, ArcheryServer, HttpTls, ProviderKind},
    providers::archery::{AuthSlot, normalize_server},
};
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn server(base_url: String) -> ArcheryServer {
    ArcheryServer {
        id: Uuid::new_v4(),
        name: "test".into(),
        base_url,
        username: "tester".into(),
        http_tls: HttpTls::default(),
        totp_mode: Default::default(),
        revision: 1,
    }
}

// 每个测试自有 loopback 服务，绝不会访问真实实例。
async fn mock(challenge: bool) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/prefix/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut buf = [0u8; 4096];
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
                let request = String::from_utf8_lossy(&data);
                let first = request.lines().next().unwrap();
                let (extra, body) = if first.starts_with("GET /prefix/login/ ") {
                    (
                        concat!("Set-Cookie: csrftoken", "=testcsrf; Path=/prefix/\r\n"),
                        "login".to_owned(),
                    )
                } else if first.starts_with("POST /prefix/authenticate/ ") {
                    assert!(
                        request
                            .to_ascii_lowercase()
                            .contains(concat!("x-csrftoken: ", "testcsrf"))
                    );
                    (
                        "",
                        if request.contains("password=wrong") {
                            r#"{"status":1,"msg":"invalid password"}"#.into()
                        } else if challenge {
                            r#"{"status":0,"data":"temporarysession"}"#.into()
                        } else {
                            r#"{"status":0,"data":null}"#.into()
                        },
                    )
                } else if first.starts_with("GET /prefix/login/2fa/ ") {
                    assert!(request.contains("sessionid=temporarysession"));
                    ("", "if ('verify_only' === 'verify_config') {} let auth_types = [{'code': 'totp'}];".into())
                } else if first.starts_with("POST /prefix/api/v1/user/2fa/verify/ ") {
                    (
                        "Set-Cookie: sessionid=ready; Path=/prefix/\r\n",
                        r#"{"status":0}"#.into(),
                    )
                } else if first.starts_with("GET /prefix/group/user_all_instances/") {
                    ("", r#"{"status":0,"data":[{"id":7,"instance_name":"test-instance","db_type":"mysql"}]}"#.into())
                } else if first.starts_with("GET /prefix/instance/instance_resource/") {
                    ("", r#"{"status":0,"data":["test_db"]}"#.into())
                } else if first.starts_with("GET /prefix/query/querylog/") {
                    ("", r#"{"total":1,"rows":[{"id":1}]}"#.into())
                } else {
                    panic!("unexpected request: {first}")
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });
    (base, task)
}

#[test]
fn service_identity_validation_and_prefix() {
    let mut s = server("http://localhost:80/platform".into());
    normalize_server(&mut s).unwrap();
    assert_eq!(s.base_url, "http://localhost/platform/");
    for invalid in [
        "http://user:secret@localhost/",
        "http://localhost/?x=1",
        "http://localhost/#x",
        "file:///tmp/test",
    ] {
        assert!(normalize_server(&mut server(invalid.into())).is_err());
    }
}

#[tokio::test]
async fn authenticates_and_checks_platform_binding() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (base, task) = mock(false).await;
    let auth = Arc::new(AuthSlot::new(server(base)).unwrap());
    let cancel = CancellationToken::new();
    let status = auth.authenticate("password".into(), &cancel).await.unwrap();
    assert_eq!(status.state, "ready");
    assert_eq!(status.generation, 1);
    let instances = auth.instances(&cancel).await.unwrap();
    assert_eq!(instances.len(), 1);
    assert_eq!(
        auth.databases(&instances[0], &cancel).await.unwrap(),
        vec!["test_db"]
    );
    let wrong = ArcheryInstance {
        id: 7,
        name: "replaced".into(),
        provider: ProviderKind::Mysql,
    };
    assert_eq!(
        auth.databases(&wrong, &cancel).await.unwrap_err().code,
        "target_changed"
    );
    assert_eq!(
        auth.query_history(0, 20, "", &cancel).await.unwrap()["total"],
        1
    );
    auth.close().await;
    assert_eq!(
        auth.instances(&cancel).await.unwrap_err().code,
        "auth_required"
    );
    task.abort();
}

#[tokio::test]
async fn challenge_is_not_ready_until_otp() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (base, task) = mock(true).await;
    let auth = AuthSlot::new(server(base)).unwrap();
    let cancel = CancellationToken::new();
    let status = auth.authenticate("password".into(), &cancel).await.unwrap();
    assert_eq!(status.state, "challenge_required");
    assert_eq!(status.generation, 0);
    assert_eq!(
        auth.instances(&cancel).await.unwrap_err().code,
        "auth_required"
    );
    let status = auth.verify("123456".into(), &cancel).await.unwrap();
    assert_eq!(status.state, "ready");
    assert_eq!(status.generation, 1);
    task.abort();
}

#[tokio::test]
async fn explicit_password_replaces_existing_challenge_only_after_authentication() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (base, task) = mock(true).await;
    let auth = AuthSlot::new(server(base)).unwrap();
    let cancel = CancellationToken::new();
    assert_eq!(
        auth.authenticate("password".into(), &cancel)
            .await
            .unwrap()
            .state,
        "challenge_required"
    );
    assert_eq!(
        auth.authenticate("wrong".into(), &cancel)
            .await
            .unwrap_err()
            .code,
        "auth_failed"
    );
    assert_eq!(
        auth.verify("123456".into(), &cancel)
            .await
            .unwrap_err()
            .code,
        "auth_required"
    );
    task.abort();
}

#[tokio::test]
async fn cancelled_auth_never_publishes_ready() {
    let (base, task) = mock(false).await;
    let auth = AuthSlot::new(server(base)).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        auth.authenticate("password".into(), &cancel)
            .await
            .unwrap_err()
            .code,
        "cancelled"
    );
    assert_ne!(auth.status().await.state, "ready");
    task.abort();
}

#[tokio::test]
async fn login_redirect_outside_service_is_rejected() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/prefix/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut data = [0u8; 4096];
        let n = stream.read(&mut data).await.unwrap();
        assert!(String::from_utf8_lossy(&data[..n]).starts_with("GET /prefix/login/ "));
        stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://example.invalid/login/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
    });
    let auth = AuthSlot::new(server(base)).unwrap();
    let error = auth
        .authenticate("password".into(), &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, "interactive_login_required");
    task.await.unwrap();
}

/// 只读取调用方显式配置的隔离 Archery 测试服务和实例。
#[tokio::test]
#[ignore]
async fn real_archery_discovery() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = std::env::var("DATAPIER_TEST_ARCHERY_URL").expect("设置隔离测试服务 URL");
    let user = std::env::var("DATAPIER_TEST_ARCHERY_USER").expect("设置隔离测试用户");
    let password = std::env::var("DATAPIER_TEST_ARCHERY_PASSWORD").expect("设置测试密码");
    let instance_id: i64 = std::env::var("DATAPIER_TEST_ARCHERY_INSTANCE_ID")
        .expect("设置隔离测试实例 ID")
        .parse()
        .unwrap();
    let mut config = server(base);
    config.username = user;
    let auth = AuthSlot::new(config).unwrap();
    let cancel = CancellationToken::new();
    let mut status = auth.authenticate(password, &cancel).await.unwrap();
    if status.state == "challenge_required" {
        status = auth
            .verify(
                std::env::var("DATAPIER_TEST_ARCHERY_OTP").expect("设置测试 OTP"),
                &cancel,
            )
            .await
            .unwrap();
    }
    assert_eq!(status.state, "ready");
    let instance = auth
        .instances(&cancel)
        .await
        .unwrap()
        .into_iter()
        .find(|i| i.id == instance_id)
        .expect("显式测试实例不可见");
    auth.databases(&instance, &cancel).await.unwrap();
}
