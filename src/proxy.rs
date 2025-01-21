// proxy.rs
use crate::config::ServiceConfig;
use crate::container::INSTANCE_STORE;
use crate::metrics::{SERVICE_REQUEST_DURATION, SERVICE_REQUEST_TOTAL, TOTAL_REQUESTS};
use crate::status::update_instance_store_cache;
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
use std::collections::{BTreeSet, HashMap};
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
        let upstream = self.loadbalancer.select(b"", 256).unwrap();

        Ok(Box::new(HttpPeer::new(
            upstream,
            false,
            "host.name".to_string(),
        )))
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

    // Check if the server already exists
    if let Some(backends) = server_backends.get(&service_name) {
        slog::debug!(
            log,
            "Updating backends for existing server: {}",
            service_name
        );

        if let Some(instances) = INSTANCE_STORE
            .get()
            .expect("Instance store not initialised")
            .get(&service_name)
        {
            for (_, metadata) in instances.value().iter() {
                if metadata.exposed_port > 0 {
                    let addr = format!("127.0.0.1:{}", metadata.exposed_port);
                    let backend = Backend::new(&addr).expect("Failed to create backend");
                    backends.insert(backend);
                }

                let valid_addresses: Vec<_> = instances
                    .value()
                    .iter()
                    .map(|(_, metadata)| format!("127.0.0.1:{}", metadata.exposed_port))
                    .collect();
                backends.retain(|backend| valid_addresses.contains(&backend.addr.to_string()));
            }
        }

        update_instance_store_cache();

        return;
    }

    // Initialise backends store for this service
    let backends = Arc::new(DashSet::new());
    server_backends.insert(service_name.clone(), backends.clone());

    // Populate initial backends from instance store
    if let Some(instances) = INSTANCE_STORE
        .get()
        .expect("Instance store not initialised")
        .get(&service_name)
    {
        for (_, metadata) in instances.value().iter() {
            if metadata.exposed_port > 0 {
                let addr = format!("127.0.0.1:{}", metadata.exposed_port);
                let backend = Backend::new(&addr).expect("Failed to create backend");
                backends.insert(backend);
            }
        }
    }

    slog::debug!(
        log,
        "Initial upstreams for {}: {:?}",
        service_name,
        backends.iter().map(|b| b.addr.clone()).collect::<Vec<_>>()
    );

    // Create discovery and load balancer
    let discovery = Discovery(backends.clone());
    let mut loadbalancer = LoadBalancer::from_backends(Backends::new(Box::new(discovery)));
    loadbalancer.update_frequency = Some(Duration::from_secs(1));

    let hc = health_check::TcpHealthCheck::new();
    loadbalancer.set_health_check(hc);
    loadbalancer.health_check_frequency = Some(Duration::from_secs(1));

    // Create background service for load balancer
    let bg_service = background_service("lb service", loadbalancer);

    // Configure proxy application
    let app = ProxyApp {
        loadbalancer: bg_service.task(),
        service_name: service_name.clone(),
    };

    let mut router_service = http_proxy_service(&Server::new(None).unwrap().configuration, app);
    router_service.add_tcp(&format!("0.0.0.0:{}", config.exposed_port));

    let mut server = Server::new(None).expect("Failed to initialise Pingora server");
    server.bootstrap();
    server.add_service(router_service);
    server.add_service(bg_service);

    // Log updates to upstreams periodically
    // task::spawn({
    //     let service_name = service_name.clone();
    //     async move {
    //         loop {
    //             sleep(Duration::from_secs(5)).await;
    //             let upstreams: Vec<_> = backends.iter().map(|b| b.addr.clone()).collect();
    //             println!("Updated upstreams for {}: {:?}", service_name, upstreams);
    //         }
    //     }
    // });

    // Run server
    let handle = task::spawn_blocking(move || {
        server.run_forever();
        // println!("Server for {} stopped", service_name);
    });

    // Store server task
    server_tasks.insert(service_name.clone(), handle);
    // Update the instance store cache
    update_instance_store_cache();

    slog::debug!(
        log,
        "Proxy for {} is running on port {}",
        service_name,
        config.exposed_port
    );
}
