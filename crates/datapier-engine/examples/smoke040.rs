#[tokio::main]
async fn main() -> datapier_core::Result<()> {
    let root =
        std::path::PathBuf::from(std::env::args_os().nth(1).expect("isolated data directory"));
    let server = tokio::spawn(datapier_engine::run(root.clone()));
    let options = datapier_engine::ClientOptions {
        start_engine: false,
        ..Default::default()
    };
    let mut connected = None;
    for _ in 0..100 {
        match datapier_engine::Client::connect_with(&root, options.clone()).await {
            Ok(client) => {
                connected = Some(client);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    let client = connected.expect("engine handshake did not become ready");
    println!("DATAPIER040_ENGINE_READY");
    let _client = client;
    server.await.expect("engine runtime task")
}
