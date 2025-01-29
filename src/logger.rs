// src/logger.rs
pub static LOG: OnceLock<slog::Logger> = OnceLock::new();

extern crate slog_async;
extern crate slog_json;

use slog::Drain;
use std::env;
use std::sync::OnceLock;

pub use slog;
pub use slog::Logger;
pub use slog_scope;

macro_rules! crate_name {
    () => {
        env!("CARGO_PKG_NAME")
    };
}

macro_rules! crate_version {
    () => {
        env!("CARGO_PKG_VERSION")
    };
}

pub fn setup_logger(log_level: String) {
    let drain = slog_json::Json::new(std::io::stderr())
        .add_default_keys()
        .build()
        .fuse();

    let service = crate_name!();
    let version = crate_version!();
    let drain = slog_async::Async::new(drain)
        .build()
        .filter_level(get_log_level(log_level))
        .fuse();

    let log = slog::Logger::root(drain, slog::o!( "svc" => service, "version" => version ));

    // Create the logger with the JSON drain as the output
    let logger = Logger::root(log.fuse(), slog::o!());
    let _guard = slog_scope::set_global_logger(logger);
    _guard.cancel_reset()
}

// get_log_level from LOG_LVL env else default to INFO
pub fn get_log_level(log_level: String) -> slog::Level {
    let log_level = if !log_level.is_empty() {
        log_level
    } else {
        env::var("LOG_LVL").unwrap_or_else(|_| String::from("INFO"))
    };

    match log_level.to_uppercase().as_str() {
        "INFO" => slog::Level::Info,
        "DEBUG" => slog::Level::Debug,
        "WARNING" => slog::Level::Warning,
        "ERROR" => slog::Level::Error,
        "TRACE" => slog::Level::Trace,
        "CRITICAL" => slog::Level::Critical,
        _ => slog::Level::Info,
    }
}
