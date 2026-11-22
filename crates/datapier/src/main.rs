//! 单一可执行文件：普通终端配置、Tern 工作台、MCP stdio 与共享 Engine。

use clap::{Parser, Subcommand};
use datapier_core::Result;
use std::path::PathBuf;
mod plugin_update;
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
    #[arg(long, global = true, hide = true)]
    data_dir_b64: Option<String>,
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
        bridge_stdin: bool,
    },
    /// 为标准 MCP client 提供本机 stdio 接口
    Mcp {
        #[arg(long = "allow-connection")]
        allow_connections: Vec<Uuid>,
        #[arg(long = "project-root")]
        project_roots: Vec<PathBuf>,
        #[arg(long)]
        environment: Option<String>,
    },
    /// 启动本机共享数据库内核
    Engine,
    /// 检查和管理已安装的 plugin 更新
    PluginUpdate {
        #[arg(long, value_parser = ["check", "download", "schedule", "apply", "status"])]
        action: String,
        #[arg(long)]
        plugin_dir: PathBuf,
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        force: bool,
        #[arg(long, hide = true)]
        worker: bool,
        #[arg(long, hide = true, requires = "worker")]
        handoff_id: Option<Uuid>,
    },
    #[command(hide = true)]
    UpdateLockProbe,
    /// 在独立 PTY 中启动已确认的外部编辑器
    #[command(hide = true)]
    ValueEditor {
        #[arg(long)]
        handoff: String,
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
    let root = if let Some(encoded) = args.data_dir_b64 {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|error| datapier_core::AppError::invalid(error.to_string()))?;
        let path = PathBuf::from(
            String::from_utf8(bytes)
                .map_err(|error| datapier_core::AppError::invalid(error.to_string()))?,
        );
        if !path.is_absolute() {
            return Err(datapier_core::AppError::invalid("数据目录必须是绝对路径"));
        }
        path
    } else {
        match args.data_dir {
            Some(path) => path,
            None => datapier_engine::default_data_dir()?,
        }
    };
    let root = std::path::absolute(root)?;
    match args.command {
        Command::Tsp {
            workspace,
            window_bridge: false,
            ..
        } => datapier_tsp::run(root, workspace),
        command => {
            // MCP stdout 仅承载协议；窗口桥接每行输出一个 JSON 回执。
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
                    Command::ValueEditor { handoff } => value_editor::run(root, handoff).await,
                    Command::Tui { workspace } => datapier_tui::run(root, workspace).await,
                    Command::Mcp {
                        allow_connections,
                        project_roots,
                        environment,
                    } => {
                        datapier_mcp::run(root, allow_connections, project_roots, environment).await
                    }
                    Command::PluginUpdate {
                        action,
                        plugin_dir,
                        state_dir,
                        force,
                        worker,
                        handoff_id,
                    } => {
                        let value = plugin_update::run(
                            &action, plugin_dir, state_dir, root, force, worker, handoff_id,
                        )
                        .await;
                        println!("{}", serde_json::to_string(&value)?);
                        Ok(())
                    }
                    Command::UpdateLockProbe => plugin_update::probe(&root),
                    Command::Engine => datapier_engine::run(root).await,
                    Command::Tsp { bridge_stdin, .. } => {
                        use std::io::{BufRead as _, Read as _, Write as _};
                        if !bridge_stdin {
                            return Err(datapier_core::AppError::invalid(
                                "窗口桥接需要 stdin 批量请求",
                            ));
                        }
                        let stdin = std::io::stdin();
                        let stdout = std::io::stdout();
                        let mut input = stdin.lock();
                        let mut output = stdout.lock();
                        let mut line = Vec::new();
                        loop {
                            line.clear();
                            if (&mut input).take(65_538).read_until(b'\n', &mut line)? == 0 {
                                break;
                            }
                            if line.last() == Some(&b'\n') {
                                line.pop();
                            }
                            if line.len() > 65_536 {
                                return Err(datapier_core::AppError::invalid(
                                    "窗口桥接请求超过 64 KiB",
                                ));
                            }
                            let payload = serde_json::from_slice(&line)?;
                            let response = datapier_tsp::window_bridge(payload).await?;
                            serde_json::to_writer(&mut output, &response)?;
                            output.write_all(b"\n")?;
                            output.flush()?;
                        }
                        Ok(())
                    }
                }
            })
        }
    }
}
