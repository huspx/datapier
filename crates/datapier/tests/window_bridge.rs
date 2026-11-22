use datapier_core::model::ClientKind;
use datapier_engine::{Client, ClientOptions};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

struct IsolatedEngine {
    child: Child,
    root: PathBuf,
}

impl Drop for IsolatedEngine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn batch_bridge_preserves_independent_pane_bindings() {
    let root = std::env::temp_dir().join(format!("datapier-window-bridge-{}", Uuid::new_v4()));
    let engine = IsolatedEngine {
        child: Command::new(env!("CARGO_BIN_EXE_datapier"))
            .arg("--data-dir")
            .arg(&root)
            .arg("engine")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
        root,
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !engine.root.join("engine.json").exists() {
        assert!(Instant::now() < deadline, "隔离 Engine 未能启动");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let options = ClientOptions {
        kind: ClientKind::Tsp,
        start_engine: false,
        ..Default::default()
    };
    let first = Client::connect_with(&engine.root, options.clone())
        .await
        .unwrap();
    let second = Client::connect_with(&engine.root, options).await.unwrap();
    let first_nonce = Uuid::new_v4();
    let second_nonce = Uuid::new_v4();
    for (client, nonce) in [(&first, first_nonce), (&second, second_nonce)] {
        client
            .request("ui.window.bind", json!({"nonce":nonce}))
            .await
            .unwrap();
    }
    let mut bridge = tokio::process::Command::new(env!("CARGO_BIN_EXE_datapier"))
        .args(["tsp", "--window-bridge", "--bridge-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let requests = json!({"requests":[
        {"pane_id":"17","nonce":first_nonce,"data_dir":engine.root,"window_id":"proof-window"},
        {"pane_id":"23","nonce":second_nonce,"data_dir":engine.root,"window_id":"proof-window"},
        {"pane_id":"29","nonce":Uuid::new_v4(),"data_dir":engine.root,"window_id":"proof-window"}
    ]});
    let mut input = bridge.stdin.take().unwrap();
    let mut responses = BufReader::new(bridge.stdout.take().unwrap()).lines();
    async fn next_response(
        responses: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    ) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(15), responses.next_line())
            .await
            .expect("桥接在 stdin 仍打开时必须回执")
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }
    let mut bytes = serde_json::to_vec(&requests).unwrap();
    bytes.push(b'\n');
    input.write_all(&bytes).await.unwrap();
    let response = next_response(&mut responses).await;
    input.write_all(b"{\"requests\":[]}\n").await.unwrap();
    assert_eq!(next_response(&mut responses).await, json!({"items":[]}));
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(15), bridge.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let items = response["items"].as_array().unwrap();
    let pane = |id| items.iter().find(|item| item["pane_id"] == id).unwrap();
    assert_eq!(pane("17")["bound"], true, "{response}");
    assert_eq!(pane("23")["bound"], true, "{response}");
    assert_eq!(pane("17")["nonce"], first_nonce.to_string());
    assert_eq!(pane("23")["nonce"], second_nonce.to_string());
    assert_ne!(
        pane("17")["client_session_id"],
        pane("23")["client_session_id"]
    );
    assert_eq!(pane("29")["bound"], false);
    first.close().await.unwrap();
    second.close().await.unwrap();
}
