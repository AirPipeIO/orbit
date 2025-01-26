// src/proxy.rs
use crate::api::status::update_instance_store_cache;
use crate::config::ServiceConfig;
use crate::container::INSTANCE_STORE;
use crate::metrics::{SERVICE_REQUEST_DURATION, SERVICE_REQUEST_TOTAL, TOTAL_REQUESTS};
use async_trait::async_trait;
use dashmap::DashMap;
use dashmap::DashSet;
use pingora::lb::discovery::ServiceDiscovery;
use pingora::lb::{Backend, Backends, LoadBalancer};
use pingora::prelude::RoundRobin;
use pingora::proxy::{http_proxy_service, ProxyHttp, Session};
use pingora::server::Server;
use pingora::services::background::background_service;
use pingora::upstreams::peer::HttpPeer;
use pingora_http::ResponseHeader;
use pingora_load_balancing::health_check;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use std::time::Instant;
use tokio::task;

pub struct Discovery(Arc<DashSet<Backend>>);

#[async_trait]
impl ServiceDiscovery for Discovery {
    async fn discover(&self) -> pingora::Result<(BTreeSet<Backend>, HashMap<u64, bool>)> {
        let mut backends = BTreeSet::new();
        for backend in self.0.iter() {
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

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Instant,
    ) -> pingora::Result<Box<HttpPeer>> {
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

    async fn response_filter(
        &self,
        _session: &mut Session,
        response: &mut ResponseHeader,
        ctx: &mut Instant,
    ) -> pingora::Result<()> {
        // Get response status
        let status = response.status.as_u16().to_string();

        // Calculate request duration
        let duration = ctx.elapsed().as_secs_f64();

        // Update metrics
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
}

// Global OnceLock for storing server instances and backends
pub static SERVER_TASKS: OnceLock<DashMap<String, task::JoinHandle<()>>> = OnceLock::new();
pub static SERVER_BACKENDS: OnceLock<DashMap<String, Arc<DashSet<Backend>>>> = OnceLock::new();

pub async fn run_proxy_for_service(service_name: String, config: ServiceConfig) {
    let log: slog::Logger = slog_scope::logger();
    let server_tasks = SERVER_TASKS.get_or_init(DashMap::new);
    let server_backends = SERVER_BACKENDS.get_or_init(DashMap::new);

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
    for (node_port, container_port) in service_ports {
        let proxy_key = format!("{}_{}", service_name, node_port);

        if let Some(backends) = server_backends.get(&proxy_key) {
            if let Some(instances) = INSTANCE_STORE.get().unwrap().get(&service_name) {
                for (_, metadata) in instances.value().iter() {
                    for container in &metadata.containers {
                        for port_info in &container.ports {
                            if let Some(container_node_port) = port_info.node_port {
                                if container_node_port == node_port {
                                    let addr =
                                        format!("{}:{}", container.ip_address, port_info.port);
                                    if let Ok(backend) = Backend::new(&addr) {
                                        backends.insert(backend);
                                        slog::debug!(log, "Added backend";
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

        // Initialize backends for this service-port
        let backends = Arc::new(DashSet::new());
        server_backends.insert(proxy_key.clone(), backends.clone());

        // Add initial backends
        if let Some(instances) = INSTANCE_STORE.get().unwrap().get(&service_name) {
            for (_, metadata) in instances.value().iter() {
                for container in &metadata.containers {
                    for port_info in &container.ports {
                        if let Some(container_node_port) = port_info.node_port {
                            if container_node_port == node_port {
                                let addr = format!("{}:{}", container.ip_address, port_info.port);
                                if let Ok(backend) = Backend::new(&addr) {
                                    backends.insert(backend);
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
        router_service.add_tcp(&format!("0.0.0.0:{}", node_port));

        let mut server = Server::new(None).expect("Failed to initialise Pingora server");
        server.bootstrap();
        server.add_service(router_service);
        server.add_service(bg_service);

        let handle = task::spawn_blocking(move || {
            server.run_forever();
        });

        server_tasks.insert(proxy_key.clone(), handle);
    }

    let _ = update_instance_store_cache();
}
