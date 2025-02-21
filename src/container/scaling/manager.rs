// src/container/scaling/manager.rs
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::config::{PodStats, ResourceThresholds, ServiceConfig};
use crate::container::scaling::codel::CoDelMetrics;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScalingPolicy {
    /// Duration to wait between scaling actions
    #[serde(
        with = "humantime_serde",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub cooldown_duration: Option<Duration>,

    /// CPU/Memory threshold percentage below which scale down is considered
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale_down_threshold_percentage: Option<f64>,
}

fn default_cooldown_duration() -> Duration {
    Duration::from_secs(60)
}

fn default_scale_down_threshold() -> f64 {
    50.0
}

impl ScalingPolicy {
    pub fn get_cooldown_duration(&self) -> Duration {
        self.cooldown_duration
            .unwrap_or_else(default_cooldown_duration)
    }

    pub fn get_scale_down_threshold(&self) -> f64 {
        self.scale_down_threshold_percentage
            .unwrap_or_else(default_scale_down_threshold)
    }
}

#[derive(Debug, Clone)]
pub enum ScalingDecision {
    ScaleUp(u32),
    ScaleDown(u32),
    NoChange,
}

#[derive(Debug, Clone)]
enum ScalingState {
    Normal,
    CoDelScalingUp { since: Instant, last_scale: Instant },
    ResourceScalingDown { since: Instant },
    Cooldown { until: Instant },
}

pub struct UnifiedScalingManager {
    service_name: String,
    state: ScalingState,
    codel_metrics: Option<Arc<Mutex<CoDelMetrics>>>,
    resource_thresholds: Option<ResourceThresholds>,
    config: ServiceConfig,
    policy: ScalingPolicy,
    last_scale_time: Instant,
}

impl UnifiedScalingManager {
    pub fn new(
        service_name: String,
        config: ServiceConfig,
        codel_metrics: Option<Arc<Mutex<CoDelMetrics>>>,
        policy: Option<ScalingPolicy>,
    ) -> Self {
        Self {
            service_name,
            state: ScalingState::Normal,
            codel_metrics,
            resource_thresholds: config.resource_thresholds.clone(),
            config,
            policy: policy.unwrap_or_default(),
            last_scale_time: Instant::now(),
        }
    }

    pub async fn evaluate(
        &mut self,
        current_instances: usize,
        pod_stats: &HashMap<Uuid, PodStats>,
    ) -> ScalingDecision {
        let now = Instant::now();

        // First check if we're in cooldown
        if now.duration_since(self.last_scale_time) < self.policy.get_cooldown_duration() {
            slog::debug!(slog_scope::logger(), "In cooldown period";
                "service" => &self.service_name,
                "remaining_secs" => (self.policy.get_cooldown_duration() - now.duration_since(self.last_scale_time)).as_secs()
            );
            return ScalingDecision::NoChange;
        }

        // If we have CoDel metrics, check them first
        if let Some(codel) = &self.codel_metrics {
            let mut metrics = codel.lock().await;
            metrics.check_traffic();

            // Check if CoDel indicates we can scale down
            if metrics.can_scale_down() {
                if current_instances > self.config.instance_count.min as usize {
                    slog::info!(slog_scope::logger(), "CoDel indicates scale down";
                        "service" => &self.service_name,
                        "current_instances" => current_instances,
                        "min_instances" => self.config.instance_count.min
                    );
                    self.last_scale_time = now; // Update last scale time
                    return ScalingDecision::ScaleDown(1);
                } else {
                    slog::debug!(slog_scope::logger(), "At minimum instances, cannot scale down";
                        "service" => &self.service_name,
                        "current_instances" => current_instances,
                        "min_instances" => self.config.instance_count.min
                    );
                }
            }

            // Check if we need to scale up
            if let Some(action) = metrics.check_state() {
                if current_instances < self.config.instance_count.max as usize {
                    slog::info!(slog_scope::logger(), "CoDel triggered scale up";
                        "service" => &self.service_name,
                        "instances" => action.instances
                    );
                    self.state = ScalingState::CoDelScalingUp {
                        since: now,
                        last_scale: now,
                    };
                    self.last_scale_time = now; // Update last scale time
                    return ScalingDecision::ScaleUp(action.instances);
                }
            }
        }

        // Then check resource thresholds
        if let Some(decision) = self.evaluate_resources(current_instances, pod_stats).await {
            match decision {
                ScalingDecision::ScaleDown(n) => {
                    if current_instances > self.config.instance_count.min as usize {
                        if let Some(codel) = &self.codel_metrics {
                            let metrics = codel.lock().await;
                            if !metrics.can_scale_down() {
                                slog::debug!(slog_scope::logger(), "Resource scale down prevented by CoDel";
                                    "service" => &self.service_name
                                );
                                return ScalingDecision::NoChange;
                            }
                        }

                        slog::info!(slog_scope::logger(), "Resource metrics indicate scale down";
                            "service" => &self.service_name,
                            "scale_down_count" => n
                        );
                        self.last_scale_time = now; // Update last scale time
                        return ScalingDecision::ScaleDown(n);
                    }
                }
                ScalingDecision::ScaleUp(n) => {
                    if current_instances < self.config.instance_count.max as usize {
                        self.last_scale_time = now; // Update last scale time
                        return ScalingDecision::ScaleUp(n);
                    }
                }
                ScalingDecision::NoChange => {}
            }
        }

        ScalingDecision::NoChange
    }

    pub fn enter_cooldown(&mut self) {
        self.last_scale_time = Instant::now();
    }

    async fn evaluate_resources(
        &self,
        _current_instances: usize,
        pod_stats: &HashMap<Uuid, PodStats>,
    ) -> Option<ScalingDecision> {
        let thresholds = self.resource_thresholds.as_ref()?;

        let mut pods_exceeding = 0;
        let mut total_evaluated_pods = 0;

        for stats in pod_stats.values() {
            let memory_percentage = if stats.memory_limit > 0 {
                stats.memory_usage as f64 / stats.memory_limit as f64 * 100.0
            } else {
                0.0
            };

            let cpu_ready = stats.cpu_percentage > 0.0;
            let cpu_relative_ready = stats.cpu_percentage_relative > 0.0;

            if !cpu_ready || !cpu_relative_ready {
                continue;
            }

            total_evaluated_pods += 1;

            let cpu_exceeded = thresholds.cpu_percentage.map_or(false, |threshold| {
                stats.cpu_percentage >= 5.0 && stats.cpu_percentage > threshold as f64
            });

            let cpu_relative_exceeded =
                thresholds
                    .cpu_percentage_relative
                    .map_or(false, |threshold| {
                        stats.cpu_percentage_relative >= 5.0
                            && stats.cpu_percentage_relative > threshold as f64
                    });

            let memory_exceeded = thresholds.memory_percentage.map_or(false, |threshold| {
                memory_percentage >= 5.0 && memory_percentage > threshold as f64
            });

            if cpu_exceeded || cpu_relative_exceeded || memory_exceeded {
                pods_exceeding += 1;
            }
        }

        if total_evaluated_pods == 0 {
            return None;
        }

        let percentage_exceeding = (pods_exceeding as f64 / total_evaluated_pods as f64) * 100.0;

        if percentage_exceeding >= 75.0 {
            Some(ScalingDecision::ScaleUp(1))
        } else if percentage_exceeding <= self.policy.get_scale_down_threshold() {
            Some(ScalingDecision::ScaleDown(1))
        } else {
            Some(ScalingDecision::NoChange)
        }
    }

    pub fn get_state(&self) -> String {
        match &self.state {
            ScalingState::Normal => "normal".to_string(),
            ScalingState::CoDelScalingUp { since, last_scale } => {
                format!(
                    "codel_scaling_up_{}",
                    last_scale.duration_since(*since).as_secs()
                )
            }
            ScalingState::ResourceScalingDown { since } => {
                format!(
                    "resource_scaling_down_{}",
                    since.duration_since(Instant::now()).as_secs()
                )
            }
            ScalingState::Cooldown { until } => {
                format!(
                    "cooldown_{}",
                    until.duration_since(Instant::now()).as_secs()
                )
            }
        }
    }
}
