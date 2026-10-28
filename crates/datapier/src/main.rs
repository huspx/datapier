//! 单一可执行文件的入口：仅分派 UI／Engine，数据库业务留在 Core 和 Engine。

use clap::{Parser, Subcommand};
use datapier_core::Result;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "datapier", version, about = "Tern 原生 MySQL / Redis 工作台")]
struct Arguments {
    #[arg(long, global = true, env = "DATAPIER_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// 打开 Tern TSP 工作台
    Ui {
        #[arg(long, default_value = "default")]
        workspace: String,
    },
    /// 启动本机共享数据库内核
    Engine,
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let args = Arguments::parse();
    let root = match args.data_dir {
        Some(path) => path,
        None => datapier_engine::default_data_dir()?,
    };
    match args.command {
        Command::Ui { workspace } => datapier_tsp::run(root, workspace),
        Command::Engine => {
            // stdout 可能承载调用方协议，日志固定写 stderr；Engine 子进程的 stdio 由平台层隔离。
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                .with_writer(std::io::stderr)
                .init();
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(datapier_engine::run(root))
        }
    }
}
