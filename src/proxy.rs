// src/proxy.rs
use crate::config::{get_config_by_service, ServiceConfig};
use crate::container::scaling::codel::get_service_metrics;
use crate::container::scaling::scale_up;
use crate::container::{INSTANCE_STORE, RUNTIME};
use crate::metrics::{SERVICE_REQUEST_DURATION, SERVICE_REQUEST_TOTAL, TOTAL_REQUESTS};
use async_trait::async_trait;
use pingora::http::ResponseHeader;
use pingora::lb::discovery::ServiceDiscovery;
use pingora::lb::{Backend, Backends, LoadBalancer};
use pingora::prelude::RoundRobin;
use pingora::proxy::{http_proxy_service, ProxyHttp, Session};
use pingora::server::Server;
use pingora::services::background::background_service;
use pingora::upstreams::peer::HttpPeer;
use pingora_load_balancing::health_check;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::RwLock;

use std::collections::{BTreeSet, HashMap, HashSet};
#[cfg(unix)]
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use std::time::Instant;
use tokio::task::{self, JoinHandle};

// Global OnceLock for storing server instances and backends
pub static SERVER_TASKS: OnceLock<Arc<RwLock<FxHashMap<String, JoinHandle<()>>>>> = OnceLock::new();
pub static SERVER_BACKENDS: OnceLock<
    Arc<RwLock<FxHashMap<String, Arc<RwLock<FxHashSet<Backend>>>>>>,
> = OnceLock::new();
pub struct Discovery(Arc<RwLock<FxHashSet<Backend>>>);

#[async_trait]
impl ServiceDiscovery for Discovery {
    async fn discover(&self) -> pingora::Result<(BTreeSet<Backend>, HashMap<u64, bool>)> {
        let mut backends = BTreeSet::new();
        let backend_set = self.0.read().await;
        for backend in backend_set.iter() {
            backends.insert(backend.clone());
        }
        Ok((backends, HashMap::new()))
    }
}

pub struct ProxyApp {
    pub loadbalancer: Arc<LoadBalancer<RoundRobin>>,
    pub service_name: String,
}

#[async_trait]
impl ProxyHttp for ProxyApp {
    type CTX = Instant; // Use Instant for timing requests

    fn new_ctx(&self) -> Self::CTX {
        // Start timing the request
        Instant::now()
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        response: &mut ResponseHeader,
        ctx: &mut Instant,
    ) -> pingora::Result<()> {
        let total_time = ctx.elapsed();
        let service_name = self.service_name.split_once("__").unwrap().0;

        // Get service configuration and check CoDel metrics here since we now have the complete request time
        if let Some(config) = get_config_by_service(service_name).await {
            if let Some(codel_config) = config.codel.clone() {
                let metrics = get_service_metrics(service_name, &codel_config).await;
                let mut metrics = metrics.lock().await;

                // Record the total request time
                metrics.record_sojourn(total_time);

                // Check if we need to take action
                if let Some(_action) = metrics.check_state() {
                    // Spawn scaling as a background task
                    let config_clone = config.clone();
                    let service_name_clone = service_name.to_string();
                    let runtime = RUNTIME.get().unwrap().clone();

                    tokio::spawn(async move {
                        if let Err(e) = scale_up(&service_name_clone, config_clone, runtime).await {
                            slog::error!(slog_scope::logger(), "Failed to scale up service";
                                "service" => service_name_clone,
                                "error" => e.to_string()
                            );
                        }
                    });
                }
            }
        }

        // Original metrics recording
        let status = response.status.as_u16().to_string();
        let duration = total_time.as_secs_f64();

        if let Some(total_requests) = TOTAL_REQUESTS.get() {
            total_requests.inc();
        }

        if let Some(request_duration) = SERVICE_REQUEST_DURATION.get() {
            request_duration
                .with_label_values(&[&self.service_name, &status])
                .observe(duration);
        }

        if let Some(request_total) = SERVICE_REQUEST_TOTAL.get() {
            request_total
                .with_label_values(&[&self.service_name, &status])
                .inc();
        }

        Ok(())
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Instant,
    ) -> pingora::Result<Box<HttpPeer>> {
        let service_name = self.service_name.split_once("__").unwrap().0;

        // Check if we should reject the request based on recent metrics
        if let Some(config) = get_config_by_service(service_name).await {
            if let Some(codel_config) = config.codel.clone() {
                let metrics = get_service_metrics(service_name, &codel_config).await;
                let metrics = metrics.lock().await;

                if metrics.should_reject() {
                    if let Some(status_code) = codel_config.overload_status_code {
                        slog::debug!(slog_scope::logger(), "Rejecting request due to CoDel";
                            "service" => service_name,
                            "status_code" => status_code
                        );

                        let response = ResponseHeader::build(status_code, Some(0))?;
                        session
                            .write_response_header(Box::new(response), true)
                            .await?;

                        let error = pingora::Error {
                            etype: pingora::ErrorType::CustomCode("overloaded", status_code),
                            esource: pingora::ErrorSource::Unset,
                            retry: pingora::RetryType::Decided(false),
                            cause: None,
                            context: Some(pingora::ImmutStr::Static("Service overloaded")),
                        };
                        return Err(Box::new(error));
                    }
                }
            }
        }

        // Proceed with backend selection
        match self.loadbalancer.select(b"", 256) {
            Some(upstream) => Ok(Box::new(HttpPeer::new(
                upstream,
                false,
                "host.name".to_string(),
            ))),
            None => {
                let error = pingora::Error {
                    etype: pingora::ErrorType::CustomCode("no_upstream", 503),
                    esource: pingora::ErrorSource::Unset,
                    retry: pingora::RetryType::Decided(false),
                    cause: None,
                    context: Some(pingora::ImmutStr::Static("No upstream available")),
                };
                Err(Box::new(error))
            }
        }
    }
}

pub async fn run_proxy_for_service(service_name: String, config: ServiceConfig) {
    let log: slog::Logger = slog_scope::logger();
    let server_tasks = SERVER_TASKS.get_or_init(|| Arc::new(RwLock::new(FxHashMap::default())));
    let server_backends =
        SERVER_BACKENDS.get_or_init(|| Arc::new(RwLock::new(FxHashMap::default())));
    let instance_store = INSTANCE_STORE.get().unwrap();

    // Track only node_ports that need external access
    let mut service_ports = HashSet::new();
    for container in &config.spec.containers {
        if let Some(ports) = &container.ports {
            for port_config in ports {
                if let Some(node_port) = port_config.node_port {
                    service_ports.insert((node_port, port_config.port));
                }
            }
        }
    }

    // Only create proxies for containers requesting external access
    for (node_port, _container_port) in service_ports {
        let proxy_key = format!("{}__{}", service_name, node_port);
        let addr = format!("0.0.0.0:{}", node_port);

        // Get read lock to check for existing backends
        let backends = {
            let backends_map = server_backends.read().await;
            backends_map.get(&proxy_key).cloned()
        };

        if let Some(backends) = backends {
            // Get read lock to access instance data
            let store = instance_store.read().await;
            if let Some(instances) = store.get(&service_name) {
                for (_, metadata) in instances {
                    for container in &metadata.containers {
                        for port_info in &container.ports {
                            if let Some(container_node_port) = port_info.node_port {
                                if container_node_port == node_port {
                                    let addr =
                                        format!("{}:{}", container.ip_address, port_info.port);
                                    if let Ok(backend) = Backend::new(&addr) {
                                        let mut backend_set = backends.write().await;
                                        backend_set.insert(backend);
                                        slog::debug!(log, "Updated backend configuration";
                                            "service" => &service_name,
                                            "container" => &container.name,
                                            "ip" => &container.ip_address,
                                            "port" => port_info.port,
                                            "node_port" => container_node_port,
                                            "network" => &metadata.network
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
            continue;
        }

        // Initialize backends for this service-port with write lock
        let backends = Arc::new(RwLock::new(FxHashSet::default()));
        {
            let mut backends_map = server_backends.write().await;
            backends_map.insert(proxy_key.clone(), backends.clone());
        }
        // Add initial backends with read lock
        {
            let store = instance_store.read().await;
            if let Some(instances) = store.get(&service_name) {
                for (_, metadata) in instances {
                    for container in &metadata.containers {
                        for port_info in &container.ports {
                            if let Some(container_node_port) = port_info.node_port {
                                if container_node_port == node_port {
                                    let addr =
                                        format!("{}:{}", container.ip_address, port_info.port);
                                    if let Ok(backend) = Backend::new(&addr) {
                                        let mut backend_set = backends.write().await;
                                        backend_set.insert(backend);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Create discovery and load balancer
        let discovery = Discovery(backends.clone());
        let mut loadbalancer = LoadBalancer::from_backends(Backends::new(Box::new(discovery)));
        loadbalancer.update_frequency = Some(Duration::from_secs(1));

        let hc = health_check::TcpHealthCheck::new();
        loadbalancer.set_health_check(hc);
        loadbalancer.health_check_frequency = Some(Duration::from_secs(1));

        let bg_service = background_service("lb service", loadbalancer);
        let app = ProxyApp {
            loadbalancer: bg_service.task(),
            service_name: proxy_key.clone(),
        };

        let mut router_service = http_proxy_service(&Server::new(None).unwrap().configuration, app);
        router_service.add_tcp(&addr);

        let mut server = Server::new(None).expect("Failed to initialise Pingora server");
        server.bootstrap();
        server.add_service(router_service);
        server.add_service(bg_service);

        let handle = task::spawn_blocking(move || {
            server.run_forever();
        });

        // Store server task with write lock
        {
            let mut tasks = server_tasks.write().await;
            tasks.insert(proxy_key.clone(), handle);
        }
    }
}
