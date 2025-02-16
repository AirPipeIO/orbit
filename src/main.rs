// src/main.rs
pub mod api;
pub mod config;
pub mod container;
pub mod logger;
pub mod metrics;
pub mod proxy;

use anyhow::Result;
use axum::{routing::get, Router};
use clap::Parser;
use config::CONFIG_STORE;
use container::{
    create_runtime, health::CONTAINER_HEALTH, scaling::codel::initialize_codel_metrics,
    volumes::initialize_volume_store, CONTAINER_STATS, IMAGE_CHECK_TASKS, INSTANCE_STORE,
    NETWORK_USAGE, RUNTIME, SCALING_TASKS, SERVICE_STATS,
};
use dashmap::DashMap;
use logger::setup_logger;
use metrics::{volumes::start_volume_metrics_task, MetricsUpdate};
use proxy::{SERVER_BACKENDS, SERVER_TASKS};
use rustc_hash::FxHashMap;
use std::{fs, path::PathBuf, process, sync::Arc, time::Duration};
use tokio::sync::RwLock;

macro_rules! crate_version {
    () => {
        env!("CARGO_PKG_VERSION")
    };
}

#[derive(Parser, Debug)]
#[command(
    name = "Orbit by AirPipe.io",
    author = "AirPipe.io",
    version = crate_version!(),
    about = "Orbit Container Management",
    long_about = None,
    help_template = "{author-with-newline} \
        {about-section}\
        Version: {version}\n\n\
        {usage-heading} {usage}\n\n\
        {all-args}",
)]
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

    /// Directory for storing persistent volumes
    #[arg(long, default_value = "volumes")]
    volume_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize the global stores
    CONFIG_STORE.get_or_init(DashMap::new);
    INSTANCE_STORE.get_or_init(|| Arc::new(RwLock::new(FxHashMap::default())));
    CONTAINER_HEALTH.get_or_init(|| Arc::new(RwLock::new(FxHashMap::default())));
    SCALING_TASKS.get_or_init(DashMap::new);
    CONTAINER_STATS.get_or_init(|| DashMap::new());
    SERVICE_STATS.get_or_init(|| DashMap::new());
    SERVER_TASKS.get_or_init(DashMap::new);
    SERVER_BACKENDS.get_or_init(DashMap::new);
    IMAGE_CHECK_TASKS.get_or_init(DashMap::new);
    NETWORK_USAGE.get_or_init(DashMap::new);
    initialize_codel_metrics();

    // Parse command line arguments
    let args = Args::parse();

    // Initialize volume store with configured path
    initialize_volume_store(&args.volume_dir).await?;
    start_volume_metrics_task().await;

    setup_logger(args.log_level);
    let log = slog_scope::logger();

    // Setup logger
    slog::info!(log, "Starting";
        "config_dir" => args.config_dir.display().to_string(),
        "runtime" => args.runtime.to_string()
    );

    // Check if config directory exists, create if it doesn't
    if !args.config_dir.exists() {
        match fs::create_dir_all(&args.config_dir) {
            Ok(_) => {
                slog::info!(log, "Created configuration directory";
                    "path" => args.config_dir.display().to_string()
                );
            }
            Err(e) => {
                slog::error!(log, "Failed to create configuration directory";
                    "path" => args.config_dir.display().to_string(),
                    "error" => e.to_string()
                );
                process::exit(1);
            }
        }
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

            // Get a read lock and calculate totals
            let store = instance_store.read().await;
            let total_services = store.len();
            let total_instances: usize = store.values().map(|instances| instances.len()).sum();

            // Explicitly drop the read lock
            drop(store);
            // Send updates asynchronously
            let _ =
                metrics::send_metrics_update(MetricsUpdate::TotalServices(total_services)).await;
            let _ =
                metrics::send_metrics_update(MetricsUpdate::TotalInstances(total_instances)).await;
        }
    });

    let app = Router::new()
        .route("/status", get(api::status::get_status))
        .route("/metrics", get(metrics::metrics_handler));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:4112").await?;
    slog::info!(log, "Status server running on http://0.0.0.0:4112");

    axum::serve(listener, app).await?;
    // Keep the application running
    tokio::signal::ctrl_c().await?;
    slog::info!(log, "Shutting down");

    Ok(())
}
