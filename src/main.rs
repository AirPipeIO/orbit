pub mod config;
pub mod container;
pub mod logger;
pub mod metrics;
pub mod proxy;
pub mod scale;
pub mod status;

use anyhow::Result;
use axum::{routing::get, Router};
use clap::Parser;
use config::CONFIG_STORE;
use container::{
    create_runtime, CONTAINER_STATS, INSTANCE_STORE, PORT_RANGE, RUNTIME, SCALING_TASKS,
    SERVICE_STATS,
};
use dashmap::DashMap;
use logger::setup_logger;
use metrics::MetricsUpdate;
use proxy::{SERVER_BACKENDS, SERVER_TASKS};
use std::{path::PathBuf, process, sync::Arc, time::Duration};

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

    /// Minimum port number for dynamic port allocation
    #[arg(
        long,
        default_value = "32768",
        help = "Minimum port number for dynamic port allocation"
    )]
    port_min: u16,

    /// Maximum port number for dynamic port allocation
    #[arg(
        long,
        default_value = "61000",
        help = "Maximum port number for dynamic port allocation"
    )]
    port_max: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize the global stores
    CONFIG_STORE.get_or_init(DashMap::new);
    INSTANCE_STORE.get_or_init(DashMap::new);
    SCALING_TASKS.get_or_init(DashMap::new);
    CONTAINER_STATS.get_or_init(|| DashMap::new());
    SERVICE_STATS.get_or_init(|| DashMap::new());
    SERVER_TASKS.get_or_init(DashMap::new);
    SERVER_BACKENDS.get_or_init(DashMap::new);

    // Parse command line arguments
    let args = Args::parse();

    // Initialize port range
    PORT_RANGE
        .set(args.port_min..args.port_max)
        .expect("Failed to set port range");

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

    // Initialize background cache update
    status::initialize_background_cache_update();

    // Initialize metrics system
    let _ = metrics::initialize_metrics();

    // Start metrics collection task
    tokio::spawn(async {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        loop {
            interval.tick().await;
            let instance_store = INSTANCE_STORE
                .get()
                .expect("Instance store not initialized");

            // Collect total counts without holding locks for long
            let total_services = instance_store.len();
            let total_instances: usize =
                instance_store.iter().map(|entry| entry.value().len()).sum();

            // Send updates asynchronously
            let _ =
                metrics::send_metrics_update(MetricsUpdate::TotalServices(total_services)).await;
            let _ =
                metrics::send_metrics_update(MetricsUpdate::TotalInstances(total_instances)).await;
        }
    });

    let app = Router::new()
        .route("/status", get(status::get_status))
        .route("/metrics", get(metrics::metrics_handler));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:4112").await?;
    slog::info!(log, "Status server running on http://0.0.0.0:4112");

    axum::serve(listener, app).await?;
    // Keep the application running
    tokio::signal::ctrl_c().await?;
    slog::info!(log, "Shutting down");

    Ok(())
}
