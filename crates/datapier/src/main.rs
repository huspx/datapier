//! 单一可执行文件：普通终端配置、Tern 工作台、MCP stdio 与共享 Engine。

use clap::{Parser, Subcommand};
use datapier_core::Result;
use std::path::PathBuf;
mod value_editor;
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "datapier",
    version,
    about = "面向开发者与 Agent 的 MySQL / Redis 工作台"
)]
struct Arguments {
    #[arg(long, global = true, env = "DATAPIER_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// 在普通终端管理数据源、认证与权限
    Tui {
        #[arg(long, default_value = "default")]
        workspace: String,
    },
    /// 打开 Tern TSP 工作台
    Tsp {
        #[arg(long, default_value = "default")]
        workspace: String,
        #[arg(long, hide = true)]
        window_bridge: bool,
        #[arg(long, hide = true, requires = "window_bridge")]
        bridge_payload: Option<String>,
    },
    /// 为标准 MCP client 提供本机 stdio 接口
    Mcp {
        #[arg(long = "allow-connection")]
        allow_connections: Vec<Uuid>,
    },
    /// 启动本机共享数据库内核
    Engine,
    /// 在独立 PTY 中启动已确认的外部编辑器
    #[command(hide = true)]
    ValueEditor {
        #[arg(long)]
        handoff: String,
        #[arg(long, hide = true)]
        data_dir_b64: Option<String>,
    },
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| datapier_core::AppError::new("tls", "无法安装 Rustls ring 加密提供者"))?;
    let args = Arguments::parse();
    let root = match args.data_dir {
        Some(path) => path,
        None => datapier_engine::default_data_dir()?,
    };
    match args.command {
        Command::Tsp {
            workspace,
            window_bridge: false,
            ..
        } => datapier_tsp::run(root, workspace),
        command => {
            // MCP stdout 仅承载协议；window bridge stdout 仅输出一个 JSON 响应。
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                .with_writer(std::io::stderr)
                .try_init()
                .map_err(|error| datapier_core::AppError::new("logging", error.to_string()))?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                match command {
                    Command::ValueEditor {
                        handoff,
                        data_dir_b64,
                    } => {
                        let root = match data_dir_b64 {
                            Some(encoded) => {
                                use base64::{Engine as _, engine::general_purpose::STANDARD};
                                let bytes = STANDARD.decode(encoded).map_err(|error| {
                                    datapier_core::AppError::invalid(error.to_string())
                                })?;
                                let path =
                                    PathBuf::from(String::from_utf8(bytes).map_err(|error| {
                                        datapier_core::AppError::invalid(error.to_string())
                                    })?);
                                if !path.is_absolute() {
                                    return Err(datapier_core::AppError::invalid(
                                        "编辑器数据目录必须是绝对路径",
                                    ));
                                }
                                path
                            }
                            None => root,
                        };
                        value_editor::run(root, handoff).await
                    }
                    Command::Tui { workspace } => datapier_tui::run(root, workspace).await,
                    Command::Mcp { allow_connections } => {
                        datapier_mcp::run(root, allow_connections).await
                    }
                    Command::Engine => datapier_engine::run(root).await,
                    Command::Tsp { bridge_payload, .. } => {
                        let payload = bridge_payload
                            .map(|text| serde_json::from_str(&text))
                            .transpose()?
                            .unwrap_or_else(|| serde_json::json!({}));
                        let response = datapier_tsp::window_bridge(root, payload).await?;
                        println!("{}", serde_json::to_string(&response)?);
                        Ok(())
                    }
                }
            })
        }
    }
}
