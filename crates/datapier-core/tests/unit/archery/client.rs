use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};
use uuid::Uuid;

fn server(base_url: String) -> ArcheryServer {
    ArcheryServer {
        id: Uuid::new_v4(),
        name: "test".into(),
        base_url,
        username: "tester".into(),
        http_tls: HttpTls::default(),
        totp_mode: TotpMode::Manual,
        revision: 1,
    }
}

fn response(status: &str, headers: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn login() -> Vec<u8> {
    response(
        "200 OK",
        concat!("Set-Cookie: csrftoken", "=initial; Path=/prefix/\r\n"),
        "login",
    )
}

fn challenge() -> Vec<u8> {
    response("200 OK", "", r#"{"status":0,"data":"temporary"}"#)
}

fn otp_page() -> Vec<u8> {
    response(
        "200 OK",
        "",
        "if ('verify_only' === 'verify_config') {} [{'code': 'totp'}]",
    )
}

// 独立监听器和按连接关闭的响应使每个测试只观察自己发出的真实请求。
async fn fixture(
    responses: Vec<Vec<u8>>,
) -> (
    AuthSlot,
    mpsc::UnboundedReceiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let slot = AuthSlot::new(server(format!(
        "http://{}/prefix/",
        listener.local_addr().unwrap()
    )))
    .unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        for response in responses {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let n = stream.read(&mut buffer).await.unwrap();
                if n == 0 {
                    return;
                }
                data.extend_from_slice(&buffer[..n]);
                if let Some(end) = data.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let _ = tx.send(String::from_utf8(data).unwrap());
            // 超限时客户端允许提前断开，不要求服务端完成发送。
            let _ = stream.write_all(&response).await;
        }
    });
    (slot, rx, task)
}

async fn authenticate(slot: &AuthSlot) {
    let status = slot
        .authenticate("password".into(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(status.state, "ready");
}
#[tokio::test]
async fn cancelling_authentication_discards_challenge_but_preserves_ready_session() {
    let (slot, _, task) = fixture(vec![
        login(),
        response("200 OK", "", r#"{"status":0,"data":null}"#),
        login(),
        challenge(),
        otp_page(),
    ])
    .await;
    authenticate(&slot).await;
    slot.cancel_authentication().await;
    assert_eq!(
        (
            slot.status().await.state.as_str(),
            slot.status().await.generation
        ),
        ("ready", 1)
    );
    assert!(slot.state.lock().await.transport.is_some());

    slot.authenticate("password".into(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(slot.status().await.state, "challenge_required");
    slot.cancel_authentication().await;
    let state = slot.state.lock().await;
    assert_eq!(
        (state.status.state.as_str(), state.status.generation),
        ("disconnected", 1)
    );
    assert!(state.transport.is_none());
    assert!(state.challenge_deadline.is_none());
    drop(state);
    task.await.unwrap();
}

#[tokio::test]
async fn publishing_candidate_replaces_shared_transport_and_preserves_name_rebind() {
    let (slot, mut requests, task) = fixture(vec![
        login(),
        response(
            "200 OK",
            "Set-Cookie: sessionid=old; Path=/prefix/\r\n",
            r#"{"status":0,"data":null}"#,
        ),
        login(),
        response(
            "200 OK",
            "Set-Cookie: sessionid=new; Path=/prefix/\r\n",
            r#"{"status":0,"data":null}"#,
        ),
        response("200 OK", "", r#"{"status":0,"data":[]}"#),
    ])
    .await;
    authenticate(&slot).await;
    let candidate = AuthSlot::new(slot.server.clone()).unwrap();
    authenticate(&candidate).await;
    let mut saved = slot.server.clone();
    saved.revision += 1;

    slot.publish_candidate(&candidate, &saved).await.unwrap();
    let status = slot.status().await;
    assert_eq!((status.state.as_str(), status.generation), ("ready", 2));
    slot.get("group/user_all_instances/", &[], &CancellationToken::new())
        .await
        .unwrap();
    for _ in 0..4 {
        requests.recv().await.unwrap();
    }
    assert!(requests.recv().await.unwrap().contains("sessionid=new"));
    slot.rebind_revision(saved.revision + 1).await.unwrap();
    assert_eq!(slot.status().await.generation, 2);
    task.await.unwrap();
}

#[tokio::test]
async fn publishing_candidate_rejects_unready_or_mismatched_configuration() {
    let (slot, _, task) = fixture(vec![
        login(),
        response("200 OK", "", r#"{"status":0,"data":null}"#),
    ])
    .await;
    authenticate(&slot).await;
    let candidate = AuthSlot::new(slot.server.clone()).unwrap();
    let mut saved = slot.server.clone();
    saved.revision += 1;
    assert_eq!(
        slot.publish_candidate(&candidate, &saved)
            .await
            .unwrap_err()
            .code,
        "auth_required"
    );
    saved.username.push('x');
    assert_eq!(
        slot.publish_candidate(&candidate, &saved)
            .await
            .unwrap_err()
            .code,
        "conflict"
    );
    assert_eq!(slot.status().await.generation, 1);
    task.await.unwrap();
}

#[tokio::test]
async fn verify_without_challenge_preserves_ready_transport_and_other_states() {
    let (slot, mut requests, task) = fixture(vec![
        login(),
        response(
            "200 OK",
            "Set-Cookie: sessionid=ready; Path=/prefix/\r\n",
            r#"{"status":0,"data":null}"#,
        ),
        response("200 OK", "", r#"{"status":0,"data":[]}"#),
    ])
    .await;
    let cancel = CancellationToken::new();
    assert_eq!(
        slot.verify("123456".into(), &cancel)
            .await
            .unwrap_err()
            .code,
        "auth_required"
    );
    assert_eq!(slot.status().await.state, "disconnected");
    authenticate(&slot).await;
    assert_eq!(
        slot.verify("123456".into(), &cancel)
            .await
            .unwrap_err()
            .code,
        "auth_required"
    );
    let state = slot.status().await;
    assert_eq!((state.state.as_str(), state.generation), ("ready", 1));
    slot.get("group/user_all_instances/", &[], &cancel)
        .await
        .unwrap();
    requests.recv().await.unwrap();
    requests.recv().await.unwrap();
    assert!(requests.recv().await.unwrap().contains("sessionid=ready"));
    task.await.unwrap();
    assert!(requests.recv().await.is_none());
    slot.close().await;
    assert_eq!(
        slot.verify("123456".into(), &cancel)
            .await
            .unwrap_err()
            .code,
        "auth_required"
    );
    assert_eq!(slot.status().await.state, "closed");
}

#[tokio::test]
async fn status_passively_expires_only_elapsed_challenges() {
    let (slot, mut requests, task) = fixture(vec![login(), challenge(), otp_page()]).await;
    let cancel = CancellationToken::new();
    slot.authenticate("password".into(), &cancel).await.unwrap();
    for _ in 0..3 {
        requests.recv().await.unwrap();
    }
    let state = slot.status().await;
    assert_eq!(
        (state.state.as_str(), state.generation),
        ("challenge_required", 0)
    );
    assert!(slot.state.lock().await.transport.is_some());
    assert!(requests.try_recv().is_err());

    slot.state.lock().await.challenge_deadline = Some(Instant::now() - Duration::from_secs(1));
    let state = slot.status().await;
    assert_eq!(
        (state.state.as_str(), state.generation),
        ("auth_required", 0)
    );
    let state = slot.state.lock().await;
    assert!(state.transport.is_none());
    assert!(state.challenge_deadline.is_none());
    drop(state);
    assert_eq!(
        slot.verify("123456".into(), &cancel)
            .await
            .unwrap_err()
            .code,
        "auth_required"
    );
    task.await.unwrap();
    assert!(requests.recv().await.is_none());
}

#[tokio::test]
async fn wrong_otp_preserves_challenge_without_publishing_ready() {
    let (slot, mut requests, task) = fixture(vec![
        login(),
        challenge(),
        otp_page(),
        response("200 OK", "", r#"{"status":1}"#),
        response("200 OK", "", r#"{"status":0}"#),
    ])
    .await;
    let cancel = CancellationToken::new();
    assert_eq!(
        slot.authenticate("password".into(), &cancel)
            .await
            .unwrap()
            .state,
        "challenge_required"
    );
    assert_eq!(
        slot.verify("111111".into(), &cancel)
            .await
            .unwrap_err()
            .code,
        "auth_failed"
    );
    let state = slot.status().await;
    assert_eq!(
        (state.state.as_str(), state.generation),
        ("challenge_required", 0)
    );
    assert_eq!(
        slot.instances(&cancel).await.unwrap_err().code,
        "auth_required"
    );
    let state = slot.verify("222222".into(), &cancel).await.unwrap();
    assert_eq!((state.state.as_str(), state.generation), ("ready", 1));
    for _ in 0..3 {
        requests.recv().await.unwrap();
    }
    assert!(requests.recv().await.unwrap().contains("otp=111111"));
    assert!(requests.recv().await.unwrap().contains("otp=222222"));
    task.await.unwrap();
}

#[tokio::test]
async fn expired_challenge_is_rejected_before_otp_is_sent() {
    let (slot, mut requests, task) = fixture(vec![login(), challenge(), otp_page()]).await;
    let cancel = CancellationToken::new();
    slot.authenticate("password".into(), &cancel).await.unwrap();
    // 直接设置已有私有期限，避免真实等待五分钟。
    slot.state.lock().await.challenge_deadline = Some(Instant::now() - Duration::from_secs(1));
    assert_eq!(
        slot.verify("123456".into(), &cancel)
            .await
            .unwrap_err()
            .code,
        "auth_required"
    );
    let state = slot.status().await;
    assert_eq!(
        (state.state.as_str(), state.generation),
        ("auth_required", 0)
    );
    assert!(slot.state.lock().await.transport.is_none());
    task.await.unwrap();
    for _ in 0..3 {
        requests.recv().await.unwrap();
    }
    assert!(requests.recv().await.is_none());
}

#[tokio::test]
async fn first_binding_and_unknown_two_factor_require_interaction() {
    for page in [
        "if ('verify_only' === 'bind_config') {} [{'code': 'totp'}]",
        "if ('verify_only' === 'verify_config') {} [{'code': 'webauthn'}]",
    ] {
        let (slot, _, task) =
            fixture(vec![login(), challenge(), response("200 OK", "", page)]).await;
        let error = slot
            .authenticate("password".into(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.code, "interactive_login_required");
        let state = slot.status().await;
        assert_eq!((state.state.as_str(), state.generation), ("auth_failed", 0));
        task.await.unwrap();
    }
}

// v1.8.4 将服务端选中的 TOTP 分支渲染成 OTP 标签，而不是输出认证类型变量。
const LEGACY_TOTP_PAGE: &str = r#"
<!DOCTYPE html><html><head><title>Archery - 两步验证</title></head>
<body><form class="login-form fade-in-effect" id="auth" method="post" role="form">
<label class="control-label" for="otpCode">OTP验证码</label>
<input class="form-control ng-valid ng-dirty ng-touched" id="otpCode" name="otpCode" type="text" autocomplete="off" required>
<button id="btnAuth" type="button" class="btn btn-success btn-block">验证</button>
</form><script>function authOTP(otp) { $.ajax({
type: "post", url: "/api/v1/user/2fa/verify/", dataType: "json",
data: { engineer: 'tester', otp: otp }
}); }</script></body></html>"#;

#[tokio::test]
async fn legacy_v184_totp_challenge_submits_otp_before_ready() {
    let (slot, mut requests, task) = fixture(vec![
        login(),
        challenge(),
        response("200 OK", "", LEGACY_TOTP_PAGE),
        response("200 OK", "", r#"{"status":0}"#),
    ])
    .await;
    let cancel = CancellationToken::new();
    let state = slot.authenticate("password".into(), &cancel).await.unwrap();
    assert_eq!(
        (state.state.as_str(), state.generation),
        ("challenge_required", 0)
    );
    assert_eq!(
        slot.instances(&cancel).await.unwrap_err().code,
        "auth_required"
    );
    let state = slot.verify("123456".into(), &cancel).await.unwrap();
    assert_eq!((state.state.as_str(), state.generation), ("ready", 1));
    requests.recv().await.unwrap();
    requests.recv().await.unwrap();
    let page = requests.recv().await.unwrap();
    assert!(page.starts_with("GET /prefix/login/2fa/ "));
    assert!(page.contains("sessionid=temporary"));
    let verify = requests.recv().await.unwrap();
    assert!(verify.starts_with("POST /prefix/api/v1/user/2fa/verify/ "));
    assert!(verify.contains("engineer=tester"));
    assert!(verify.contains("otp=123456"));
    assert!(!verify.contains("key="));
    task.await.unwrap();
}

#[tokio::test]
async fn legacy_non_totp_and_unrecognized_pages_require_interaction() {
    let other_type = LEGACY_TOTP_PAGE.replace("OTP验证码", "验证码").replace(
        "<button id=\"btnAuth\"",
        "<button id=\"btnCaptcha\" type=\"button\">获取验证码</button><button id=\"btnAuth\"",
    );
    let unknown = LEGACY_TOTP_PAGE.replace("OTP验证码", "未知验证码");
    let binding = LEGACY_TOTP_PAGE.replace("otp: otp", "otp: otp, key: generatedKey");
    for page in [other_type, unknown, binding] {
        let (slot, _, task) =
            fixture(vec![login(), challenge(), response("200 OK", "", &page)]).await;
        let error = slot
            .authenticate("password".into(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.code, "interactive_login_required");
        let state = slot.status().await;
        assert_eq!((state.state.as_str(), state.generation), ("auth_failed", 0));
        task.await.unwrap();
    }
}

#[tokio::test]
async fn csrf_rotation_is_used_for_business_posts() {
    let (slot, mut requests, task) = fixture(vec![
        login(),
        response(
            "200 OK",
            concat!("Set-Cookie: csrftoken", "=rotated; Path=/prefix/\r\n"),
            r#"{"status":0,"data":null}"#,
        ),
        response("200 OK", "", r#"{"status":0}"#),
    ])
    .await;
    authenticate(&slot).await;
    let value: Json = slot
        .request(
            "business/",
            &[],
            true,
            false,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(value["status"], 0);
    requests.recv().await.unwrap();
    let auth = requests.recv().await.unwrap().to_ascii_lowercase();
    assert!(auth.contains("x-csrftoken: initial\r\n"));
    let business = requests.recv().await.unwrap().to_ascii_lowercase();
    assert!(business.contains("x-csrftoken: rotated\r\n"));
    assert!(business.contains(concat!("csrftoken", "=rotated")));
    task.await.unwrap();
}

#[tokio::test]
async fn missing_csrf_stops_post_and_rejected_csrf_returns_permission_denied() {
    let (slot, mut requests, task) = fixture(vec![response("200 OK", "", "login")]).await;
    assert_eq!(
        slot.authenticate("password".into(), &CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        "remote_protocol"
    );
    task.await.unwrap();
    assert!(
        requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /prefix/login/ ")
    );
    assert!(requests.recv().await.is_none());

    let (slot, mut requests, task) = fixture(vec![
        login(),
        response("200 OK", "", r#"{"status":0,"data":null}"#),
        response("403 Forbidden", "", "CSRF failed"),
    ])
    .await;
    authenticate(&slot).await;
    let transport = slot.state.lock().await.transport.clone().unwrap();
    transport
        .jar
        .add_cookie_str(concat!("csrftoken", "=wrong; Path=/prefix/"), &slot.base);
    assert_eq!(
        slot.request::<Json>(
            "business/",
            &[],
            true,
            false,
            None,
            &CancellationToken::new()
        )
        .await
        .unwrap_err()
        .code,
        "permission_denied"
    );
    requests.recv().await.unwrap();
    requests.recv().await.unwrap();
    assert!(
        requests
            .recv()
            .await
            .unwrap()
            .to_ascii_lowercase()
            .contains("x-csrftoken: wrong\r\n")
    );
    task.await.unwrap();
}

#[tokio::test]
async fn login_redirects_cannot_escape_prefix_or_exceed_limit() {
    let (slot, mut requests, task) = fixture(vec![response(
        "302 Found",
        "Location: /outside/login/\r\n",
        "",
    )])
    .await;
    assert_eq!(
        slot.authenticate("password".into(), &CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        "interactive_login_required"
    );
    task.await.unwrap();
    assert!(
        requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /prefix/login/ ")
    );
    assert!(requests.recv().await.is_none());

    let redirects = (0..4)
        .map(|_| response("302 Found", "Location: /prefix/login/\r\n", ""))
        .collect();
    let (slot, mut requests, task) = fixture(redirects).await;
    assert_eq!(
        slot.authenticate("password".into(), &CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        "interactive_login_required"
    );
    task.await.unwrap();
    for _ in 0..4 {
        assert!(
            requests
                .recv()
                .await
                .unwrap()
                .starts_with("GET /prefix/login/ ")
        );
    }
    assert!(requests.recv().await.is_none());
}

#[tokio::test]
async fn business_redirect_html_and_statuses_have_distinct_errors() {
    for (reply, expected, state) in [
        (
            response("302 Found", "Location: /prefix/login/\r\n", ""),
            "auth_required",
            "auth_required",
        ),
        (
            response("302 Found", "Location: /prefix/other/\r\n", ""),
            "remote_protocol",
            "ready",
        ),
        (
            response(
                "200 OK",
                "Content-Type: text/html\r\n",
                "<html>login</html>",
            ),
            "remote_protocol",
            "ready",
        ),
        (
            response("429 Too Many Requests", "", ""),
            "rate_limited",
            "ready",
        ),
        (
            response("403 Forbidden", "", ""),
            "permission_denied",
            "ready",
        ),
    ] {
        let (slot, mut requests, task) = fixture(vec![
            login(),
            response("200 OK", "", r#"{"status":0,"data":null}"#),
            reply,
        ])
        .await;
        authenticate(&slot).await;
        assert_eq!(
            slot.get("business/", &[], &CancellationToken::new())
                .await
                .unwrap_err()
                .code,
            expected
        );
        assert_eq!(slot.status().await.state, state);
        task.await.unwrap();
        requests.recv().await.unwrap();
        requests.recv().await.unwrap();
        assert!(
            requests
                .recv()
                .await
                .unwrap()
                .starts_with("GET /prefix/business/")
        );
        assert!(requests.recv().await.is_none());
    }
}

#[tokio::test]
async fn real_http_response_size_and_encoding_boundaries() {
    let mut exact =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {MAX_BODY}\r\nConnection: close\r\n\r\n\"")
            .into_bytes();
    exact.resize(exact.len() + MAX_BODY - 2, b'a');
    exact.push(b'"');
    let mut chunked =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    chunked.extend_from_slice(format!("{:x}\r\n", MAX_BODY).as_bytes());
    chunked.resize(chunked.len() + MAX_BODY, b'a');
    chunked.extend_from_slice(b"\r\n1\r\nb\r\n0\r\n\r\n");
    let declared = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        MAX_BODY + 1
    )
    .into_bytes();
    let encoded = response("200 OK", "Content-Encoding: br\r\n", "compressed");
    let (slot, _, task) = fixture(vec![declared, chunked, exact, encoded]).await;
    let transport = slot.transport(&slot.server.http_tls).unwrap();
    let cancel = CancellationToken::new();
    for expected in [
        Some("response_too_large"),
        Some("response_too_large"),
        None,
        Some("remote_protocol"),
    ] {
        let reply = slot
            .send(
                transport.client.get(slot.endpoint("body/").unwrap()),
                &cancel,
            )
            .await
            .unwrap();
        match expected {
            Some(code) => assert_eq!(
                slot.decode::<String>(reply, &cancel)
                    .await
                    .unwrap_err()
                    .code,
                code
            ),
            None => assert_eq!(
                slot.decode::<String>(reply, &cancel).await.unwrap().len(),
                MAX_BODY - 2
            ),
        }
    }
    task.await.unwrap();
}

#[tokio::test]
async fn real_http_send_timeout_and_inflight_cancellation() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    for cancel_request in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let slot = AuthSlot::new(server(format!(
            "http://{}/prefix/",
            listener.local_addr().unwrap()
        )))
        .unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let _ = tx.send(());
            std::future::pending::<()>().await;
        });
        let transport = slot.transport(&slot.server.http_tls).unwrap();
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        let waiting = tokio::spawn(async move {
            rx.await.unwrap();
            if cancel_request {
                trigger.cancel();
            }
        });
        let request = transport
            .client
            .get(slot.endpoint("slow/").unwrap())
            .timeout(Duration::from_secs(2));
        let error = slot.send(request, &cancel).await.unwrap_err();
        if cancel_request {
            assert_eq!(error.code, "cancelled");
        } else {
            assert_eq!(error.code, "http");
            assert!(error.message.contains("超时"));
        }
        waiting.await.unwrap();
        server.abort();
    }
}
