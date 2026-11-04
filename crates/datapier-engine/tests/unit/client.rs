use super::*;

#[tokio::test]
async fn interleaved_events_and_responses_do_not_hold_client_open() {
    let (stream, mut peer) = tokio::io::duplex(4096);
    let task_id = Uuid::new_v4();
    let server = tokio::spawn(async move {
        let open = protocol::read(&mut peer).await.unwrap();
        protocol::write(
            &mut peer,
            &json!({"id":open["id"],"result":{"protocol_version":2}}),
        )
        .await
        .unwrap();
        let first = protocol::read(&mut peer).await.unwrap();
        let second = protocol::read(&mut peer).await.unwrap();
        protocol::write(&mut peer, &json!({"seq":1,"kind":"execution.state","task_id":task_id,"payload":{"state":"running"}})).await.unwrap();
        protocol::write(&mut peer, &json!({"id":second["id"],"result":2}))
            .await
            .unwrap();
        protocol::write(&mut peer, &json!({"seq":2,"kind":"execution.state","task_id":task_id,"payload":{"state":"succeeded"}})).await.unwrap();
        protocol::write(&mut peer, &json!({"id":first["id"],"result":1}))
            .await
            .unwrap();
        let _ = protocol::read(&mut peer).await;
    });
    let client = Client::handshake(Box::new(stream)).await.unwrap();
    let mut events = client.subscribe_events();
    let one = client.request("one", json!({}));
    let two = client.request("two", json!({}));
    let (one, two) = tokio::join!(one, two);
    assert_eq!(one.unwrap(), 1);
    assert_eq!(two.unwrap(), 2);
    assert_eq!(events.recv().await.unwrap().payload["state"], "running");
    assert_eq!(events.recv().await.unwrap().payload["state"], "succeeded");
    drop(client);
    assert!(matches!(
        events.recv().await,
        Err(broadcast::error::RecvError::Closed)
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn oversized_request_preserves_concurrent_requests_and_events() {
    let (stream, mut peer) = tokio::io::duplex(4096);
    let (first_tx, first_rx) = oneshot::channel();
    let task_id = Uuid::new_v4();
    let server = tokio::spawn(async move {
        let open = protocol::read(&mut peer).await.unwrap();
        protocol::write(
            &mut peer,
            &json!({"id":open["id"],"result":{"protocol_version":2}}),
        )
        .await
        .unwrap();
        let first = protocol::read(&mut peer).await.unwrap();
        assert_eq!(first["method"], "first");
        first_tx.send(()).unwrap();
        let second = protocol::read(&mut peer).await.unwrap();
        assert_eq!(second["method"], "second");
        protocol::write(&mut peer, &json!({"seq":1,"kind":"execution.state","task_id":task_id,"payload":{"state":"running"}})).await.unwrap();
        protocol::write(&mut peer, &json!({"id":second["id"],"result":2}))
            .await
            .unwrap();
        protocol::write(&mut peer, &json!({"id":first["id"],"result":1}))
            .await
            .unwrap();
        let _ = protocol::read(&mut peer).await;
    });
    let client = Client::handshake(Box::new(stream)).await.unwrap();
    let mut events = client.subscribe_events();
    let first_client = client.clone();
    let first = tokio::spawn(async move { first_client.request("first", json!({})).await });
    first_rx.await.unwrap();

    let error = client
        .request("oversized", json!({"data":"x".repeat(8 * 1024 * 1024)}))
        .await
        .unwrap_err();
    assert_eq!(error.code, "invalid_input");
    assert_eq!(error.message, "IPC 响应超过 8 MiB");
    assert_eq!(client.request("second", json!({})).await.unwrap(), 2);
    assert_eq!(first.await.unwrap().unwrap(), 1);
    assert_eq!(events.recv().await.unwrap().task_id, task_id);
    drop(client);
    server.await.unwrap();
}

#[tokio::test]
async fn cancelled_oversized_request_does_not_interrupt_writer() {
    use tokio::io::AsyncReadExt;

    let (stream, mut peer) = tokio::io::duplex(64);
    let (started_tx, started_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let open = protocol::read(&mut peer).await.unwrap();
        protocol::write(
            &mut peer,
            &json!({"id":open["id"],"result":{"protocol_version":2}}),
        )
        .await
        .unwrap();
        let size = peer.read_u32().await.unwrap() as usize;
        started_tx.send(()).unwrap();
        resume_rx.await.unwrap();
        let mut data = vec![0; size];
        peer.read_exact(&mut data).await.unwrap();
        let first: Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(first["method"], "first");
        protocol::write(&mut peer, &json!({"id":first["id"],"result":1}))
            .await
            .unwrap();
        let second = protocol::read(&mut peer).await.unwrap();
        assert_eq!(second["method"], "second");
        protocol::write(&mut peer, &json!({"id":second["id"],"result":2}))
            .await
            .unwrap();
        let _ = protocol::read(&mut peer).await;
    });
    let client = Client::handshake(Box::new(stream)).await.unwrap();
    let first_client = client.clone();
    let first = tokio::spawn(async move {
        first_client
            .request("first", json!({"data":"x".repeat(16384)}))
            .await
    });
    started_rx.await.unwrap();
    let mut cancelled =
        Box::pin(client.request("oversized", json!({"data":"x".repeat(8 * 1024 * 1024)})));
    tokio::select! {
        biased;
        result = &mut cancelled => panic!("oversized request unexpectedly finished: {result:?}"),
        _ = tokio::task::yield_now() => {},
    }
    drop(cancelled);
    resume_tx.send(()).unwrap();
    assert_eq!(first.await.unwrap().unwrap(), 1);
    assert_eq!(client.request("second", json!({})).await.unwrap(), 2);
    drop(client);
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_last_drops_close_a_blocked_writer() {
    use tokio::io::AsyncReadExt;

    let (stream, mut peer) = tokio::io::duplex(64);
    let handshake = tokio::spawn(Client::handshake(Box::new(stream)));
    let open = protocol::read(&mut peer).await.unwrap();
    protocol::write(
        &mut peer,
        &json!({"id":open["id"],"result":{"protocol_version":2}}),
    )
    .await
    .unwrap();
    let client = handshake.await.unwrap().unwrap();
    client
        .inner
        .send
        .send(json!({"payload":"x".repeat(16384)}))
        .await
        .unwrap();
    let length = peer.read_u32().await.unwrap() as usize;
    let mut first = [0; 1];
    peer.read_exact(&mut first).await.unwrap();

    // 已确认对端开始接收大帧，随后停止读取，保证写入受小容量缓冲区阻塞。
    let other = client.clone();
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let first_barrier = barrier.clone();
    let first_drop = tokio::spawn(async move {
        first_barrier.wait().await;
        drop(client);
    });
    let second_barrier = barrier.clone();
    let second_drop = tokio::spawn(async move {
        second_barrier.wait().await;
        drop(other);
    });
    barrier.wait().await;
    first_drop.await.unwrap();
    second_drop.await.unwrap();

    let mut remainder = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), peer.read_to_end(&mut remainder))
        .await
        .expect("释放最后句柄后写端未关闭")
        .unwrap();
    assert!(remainder.len() + 1 < length, "阻塞写入应在完整帧发送前终止");
}

#[tokio::test]
async fn dropping_last_client_closes_a_stalled_heartbeat() {
    let (stream, mut peer) = tokio::io::duplex(4096);
    let (heartbeat_tx, heartbeat_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let open = protocol::read(&mut peer).await.unwrap();
        protocol::write(
            &mut peer,
            &json!({"id":open["id"],"result":{"protocol_version":2}}),
        )
        .await
        .unwrap();
        let heartbeat = protocol::read(&mut peer).await.unwrap();
        assert_eq!(heartbeat["method"], "session.heartbeat");
        heartbeat_tx.send(()).unwrap();
        assert!(protocol::read(&mut peer).await.is_err());
    });
    let client = Client::handshake(Box::new(stream)).await.unwrap();
    let mut events = client.subscribe_events();
    tokio::time::timeout(Duration::from_secs(12), heartbeat_rx)
        .await
        .unwrap()
        .unwrap();
    drop(client);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap(),
        Err(broadcast::error::RecvError::Closed)
    ));
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn received_close_response_wins_over_disconnect() {
    let (send, mut receive) = mpsc::channel(4);
    let inner = Arc::new(Inner {
        send,
        pending: Mutex::new(HashMap::new()),
        shutdown: CancellationToken::new(),
        closed: AtomicBool::new(false),
        events: Mutex::new(Some(broadcast::channel(128).0)),
    });
    let client = Client {
        inner: inner.clone(),
        lifetime: Arc::new(ClientLifetime(inner.shutdown.clone())),
    };
    let request = tokio::spawn(async move { client.close().await });
    let message = receive.recv().await.unwrap();
    let id = message["id"].as_str().unwrap();
    let response = inner.pending.lock().unwrap().remove(id).unwrap();
    response.send(Ok(json!({"closed": true}))).unwrap();
    inner.shutdown.cancel();
    request.await.unwrap().unwrap();
}

#[tokio::test]
async fn cancelled_requests_are_removed_and_shutdown_wakes_waiters() {
    let (send, mut receive) = mpsc::channel(4);
    let inner = Arc::new(Inner {
        send,
        pending: Mutex::new(HashMap::new()),
        shutdown: CancellationToken::new(),
        closed: AtomicBool::new(false),
        events: Mutex::new(Some(broadcast::channel(128).0)),
    });
    let client = Client {
        inner: inner.clone(),
        lifetime: Arc::new(ClientLifetime(inner.shutdown.clone())),
    };
    let pending_client = client.clone();
    let task = tokio::spawn(async move { pending_client.request("slow", json!({})).await });
    receive.recv().await.unwrap();
    assert_eq!(inner.pending.lock().unwrap().len(), 1);
    task.abort();
    let _ = task.await;
    assert!(inner.pending.lock().unwrap().is_empty());

    let pending_client = client.clone();
    let task = tokio::spawn(async move { pending_client.request("slow", json!({})).await });
    receive.recv().await.unwrap();
    inner.shutdown.cancel();
    assert_eq!(task.await.unwrap().unwrap_err().code, "disconnected");
    assert!(inner.pending.lock().unwrap().is_empty());
}

#[tokio::test]
async fn client_rejects_v1_engine_and_sends_no_business_request() {
    let (stream, mut peer) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let open = protocol::read(&mut peer).await.unwrap();
        assert_eq!(open["method"], "session.open");
        assert_eq!(open["params"]["protocol_version"], 2);
        protocol::write(
            &mut peer,
            &json!({"id":open["id"],"result":{"protocol_version":1}}),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(3), protocol::read(&mut peer))
                .await
                .unwrap()
                .is_err()
        );
    });
    let error = match Client::handshake(Box::new(stream)).await {
        Ok(_) => panic!("v1 Engine must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.code, "protocol_incompatible");
    server.await.unwrap();
}
