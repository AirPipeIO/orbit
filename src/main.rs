pub mod config;
pub mod container;
pub mod logger;
pub mod proxy;
pub mod scale;
pub mod status;

use anyhow::Result;
use clap::Parser;
use config::CONFIG_STORE;
use container::{create_runtime, CONTAINER_STATS, INSTANCE_STORE, RUNTIME, SCALING_TASKS};
use dashmap::DashMap;
use logger::setup_logger;
use std::{path::PathBuf, process};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Directory containing service configuration YAML files
    #[arg(short, long, default_value = "configs")]
    config_dir: PathBuf,
    /// Container runtime
    #[arg(short, long, default_value = "docker")]
    runtime: String,
    /// Log level
    #[arg(
        short,
        long,
        default_value = "info",
        env = "LOG_LVL",
        help = "Log Levels: info, debug, warning, error, trace, critical"
    )]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize the global stores
    CONFIG_STORE.get_or_init(DashMap::new);
    INSTANCE_STORE.get_or_init(DashMap::new);
    SCALING_TASKS.get_or_init(DashMap::new);
    CONTAINER_STATS.get_or_init(|| DashMap::new());

    // Parse command line arguments
    let args = Args::parse();

    setup_logger(args.log_level);
    let log = slog_scope::logger();

    // Setup logger
    slog::info!(log, "Starting";
        "config_dir" => args.config_dir.display().to_string(),
        "runtime" => args.runtime.to_string()
    );

    // Check if config directory exists
    if !args.config_dir.exists() {
        slog::error!(log, "Configuration directory does not exist";
            "path" => args.config_dir.display().to_string()
        );
        process::exit(1);
    }

    // init container runtime
    let runtime = create_runtime(&args.runtime)?;
    RUNTIME.set(runtime).expect("Failed to set runtime");

    // Initialise existing configs
    config::initialize_configs(&args.config_dir).await?;

    tokio::spawn(async move {
        if let Err(e) = config::watch_directory(args.config_dir.to_path_buf()).await {
            let log = slog_scope::logger();
            slog::error!(log, "failed to watch directory"; "err" => &e.to_string());
        }
    });

    status::server_start().await;

    // Keep the application running
    tokio::signal::ctrl_c().await?;
    slog::info!(log, "Shutting down");

    Ok(())
}
