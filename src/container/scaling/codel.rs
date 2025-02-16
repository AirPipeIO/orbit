// src/container/scaling/codel.rs
use dashmap::DashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::config::CoDelConfig;

// Global store for CoDel metrics
pub static CODEL_METRICS: OnceLock<Arc<DashMap<String, Arc<Mutex<CoDelMetrics>>>>> =
    OnceLock::new();

#[derive(Debug, Clone)]
pub struct ScaleAction {
    pub service: String,
    pub instances: u32,
    pub status_code: Option<u16>,
}

#[derive(Debug)]
pub struct CoDelMetrics {
    /// Service name these metrics belong to
    service_name: String,

    /// Queue of packet sojourn times (time in queue) with their timestamps
    sojourn_times: VecDeque<(Instant, Duration)>,

    /// Time when we first went above target
    first_above_time: Option<Instant>,

    /// Last time we performed a scaling action
    last_scale_time: Instant,

    /// Configuration reference
    config: CoDelConfig,
}

impl CoDelMetrics {
    pub fn new(service_name: String, config: CoDelConfig) -> Self {
        Self {
            service_name,
            sojourn_times: VecDeque::new(),
            first_above_time: None,
            last_scale_time: Instant::now(),
            config,
        }
    }

    pub fn check_traffic(&mut self) {
        let now = Instant::now();
        let recent_samples = self
            .sojourn_times
            .iter()
            .filter(|(time, _)| now.duration_since(*time) <= self.config.interval)
            .count();

        if recent_samples == 0 && self.first_above_time.is_some() {
            slog::info!(slog_scope::logger(), "No recent traffic, resetting high latency state";
                "service" => &self.service_name
            );
            self.first_above_time = None;
        }
    }

    pub fn record_sojourn(&mut self, sojourn_time: Duration) {
        let now = Instant::now();

        // Add new sample with timestamp
        self.sojourn_times.push_back((now, sojourn_time));

        // Log significant latency changes
        static LAST_LOGGED_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sojourn_ms = sojourn_time.as_millis() as u64;
        let last_ms = LAST_LOGGED_MS.load(std::sync::atomic::Ordering::Relaxed);
        if (sojourn_ms as i64 - last_ms as i64).abs() > 1000 {
            slog::trace!(slog_scope::logger(), "Significant latency change";
                "service" => &self.service_name,
                "sojourn_ms" => sojourn_ms,
                "target_ms" => self.config.target.as_millis(),
                "sample_count" => self.sojourn_times.len()
            );
            LAST_LOGGED_MS.store(sojourn_ms, std::sync::atomic::Ordering::Relaxed);
        }

        // Remove old samples more aggressively when latency drops
        let max_age = if sojourn_time <= self.config.target {
            self.config.interval // Use single interval when under target
        } else {
            self.config.interval.mul_f32(2.0) // Use double interval when over target
        };

        // Remove old samples
        while let Some((sample_time, _)) = self.sojourn_times.front() {
            if now.duration_since(*sample_time) > max_age {
                self.sojourn_times.pop_front();
            } else {
                break;
            }
        }

        // Keep window size reasonable
        while self.sojourn_times.len() > 1000 {
            self.sojourn_times.pop_front();
        }

        // Log window cleanup
        if self.sojourn_times.is_empty() {
            slog::info!(slog_scope::logger(), "All samples cleared";
                "service" => &self.service_name
            );
        }
    }

    pub fn should_reject(&self) -> bool {
        if self.sojourn_times.len() < 10 {
            slog::trace!(slog_scope::logger(), "Not enough samples for rejection decision";
                "service" => &self.service_name,
                "samples" => self.sojourn_times.len()
            );
            return false;
        }

        // Only consider samples from the current interval
        let now = Instant::now();
        let recent_samples: Vec<Duration> = self
            .sojourn_times
            .iter()
            .filter(|(time, _)| now.duration_since(*time) <= self.config.interval)
            .map(|(_, duration)| *duration)
            .collect();

        if recent_samples.is_empty() {
            slog::trace!(slog_scope::logger(), "No recent samples for rejection decision";
                "service" => &self.service_name
            );
            return false;
        }

        let min_sojourn = *recent_samples.iter().min().unwrap();
        let should_reject = min_sojourn > self.config.target;

        slog::trace!(slog_scope::logger(), "Rejection decision";
            "service" => &self.service_name,
            "min_sojourn_ms" => min_sojourn.as_millis(),
            "target_ms" => self.config.target.as_millis(),
            "should_reject" => should_reject
        );

        should_reject
    }

    pub fn check_state(&mut self) -> Option<ScaleAction> {
        let now = Instant::now();

        // Get recent samples
        let recent_samples: Vec<Duration> = self
            .sojourn_times
            .iter()
            .filter(|(time, _)| now.duration_since(*time) <= self.config.interval)
            .map(|(_, duration)| *duration)
            .collect();

        // Log sample counts periodically
        static LAST_COUNT_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let current_time = now.duration_since(Instant::now()).as_secs();
        if current_time - LAST_COUNT_LOG.load(std::sync::atomic::Ordering::Relaxed) >= 10 {
            slog::info!(slog_scope::logger(), "Sample count status";
                "service" => &self.service_name,
                "total_samples" => self.sojourn_times.len(),
                "recent_samples" => recent_samples.len()
            );
            LAST_COUNT_LOG.store(current_time, std::sync::atomic::Ordering::Relaxed);
        }

        // Don't scale if we're in cooldown
        if now.duration_since(self.last_scale_time) < self.config.scale_cooldown {
            return None;
        }

        // Need minimum samples for scale up
        if recent_samples.len() < 10 {
            return None;
        }

        let min_sojourn = *recent_samples.iter().min().unwrap();
        let avg_sojourn = recent_samples.iter().sum::<Duration>() / recent_samples.len() as u32;

        // If min_sojourn is above target, we're definitely in trouble
        if min_sojourn > self.config.target {
            if self.first_above_time.is_none() {
                self.first_above_time = Some(now);
                slog::info!(slog_scope::logger(), "First detection above target";
                    "service" => &self.service_name,
                    "min_sojourn_ms" => min_sojourn.as_millis(),
                    "avg_sojourn_ms" => avg_sojourn.as_millis()
                );
                return None;
            }

            let consecutive_duration = now.duration_since(self.first_above_time.unwrap());

            if consecutive_duration
                >= self
                    .config
                    .interval
                    .mul_f32(self.config.consecutive_intervals as f32)
            {
                slog::info!(slog_scope::logger(), "Scale up condition met";
                    "service" => &self.service_name,
                    "consecutive_intervals" => self.config.consecutive_intervals,
                    "min_sojourn_ms" => min_sojourn.as_millis(),
                    "avg_sojourn_ms" => avg_sojourn.as_millis()
                );

                self.last_scale_time = now;
                self.first_above_time = None;

                return Some(ScaleAction {
                    service: self.service_name.clone(),
                    instances: 1,
                    status_code: None,
                });
            }
        } else {
            if self.first_above_time.is_some() {
                slog::info!(slog_scope::logger(), "Latency back below target";
                    "service" => &self.service_name,
                    "min_sojourn_ms" => min_sojourn.as_millis(),
                    "avg_sojourn_ms" => avg_sojourn.as_millis()
                );
                self.first_above_time = None;
            }
        }

        None
    }

    pub fn can_scale_down(&self) -> bool {
        let now = Instant::now();

        // If we have no recent samples, we should scale down
        let recent_samples: Vec<Duration> = self
            .sojourn_times
            .iter()
            .filter(|(time, _)| now.duration_since(*time) <= self.config.interval)
            .map(|(_, duration)| *duration)
            .collect();

        if recent_samples.is_empty() {
            slog::info!(slog_scope::logger(), "No recent traffic, can scale down";
                "service" => &self.service_name
            );
            return true;
        }

        if recent_samples.len() < 5 {
            // minimum sample requirement for scale down
            slog::debug!(slog_scope::logger(), "Not enough recent samples for scale down";
                "service" => &self.service_name,
                "samples" => recent_samples.len()
            );
            return false;
        }

        let max_sojourn = *recent_samples.iter().max().unwrap();
        let avg_sojourn = recent_samples.iter().sum::<Duration>() / recent_samples.len() as u32;
        let target_threshold = self.config.target.mul_f32(0.5);

        // Consider both max and average latency for scale down
        let can_scale = max_sojourn < target_threshold && avg_sojourn < target_threshold;

        // Log only when scale down status changes
        static LAST_SCALE_DOWN_STATUS: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if LAST_SCALE_DOWN_STATUS.load(std::sync::atomic::Ordering::Relaxed) != can_scale {
            slog::info!(slog_scope::logger(), "Scale down status change";
                "service" => &self.service_name,
                "can_scale_down" => can_scale,
                "max_sojourn_ms" => max_sojourn.as_millis(),
                "avg_sojourn_ms" => avg_sojourn.as_millis(),
                "target_threshold_ms" => target_threshold.as_millis(),
                "recent_samples" => recent_samples.len()
            );
            LAST_SCALE_DOWN_STATUS.store(can_scale, std::sync::atomic::Ordering::Relaxed);
        }

        can_scale
    }
}

// Function to initialize CoDel metrics tracking
pub fn initialize_codel_metrics() {
    CODEL_METRICS.get_or_init(|| Arc::new(DashMap::new()));
}

// Function to get or create metrics for a service
pub async fn get_service_metrics(
    service_name: &str,
    config: &CoDelConfig,
) -> Arc<Mutex<CoDelMetrics>> {
    let metrics_store = CODEL_METRICS.get().expect("CoDel metrics not initialized");

    if let Some(metrics) = metrics_store.get(service_name) {
        metrics.value().clone()
    } else {
        let metrics = Arc::new(Mutex::new(CoDelMetrics::new(
            service_name.to_string(),
            config.clone(),
        )));
        metrics_store.insert(service_name.to_string(), metrics.clone());
        metrics
    }
}
