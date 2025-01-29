# Orbit

## What is Orbit

Orbit is a simple, lightweight solution for scaling containers with built-in service discovery and proxying.

> **Note**: Orbit is under active development. While it's being used in production environments, features and APIs may change as we continue to improve the project. We encourage you to try it out and [provide feedback](https://github.com/airpipeio/orbit/issues/new/choose)!

## Why?
- Docker swarm has no autoscaling, Kubernetes has a large learning and management overhead and substantially larger footprint. 
- Scaling containers shouldn't need complex infra or dependencies
- [Air Pipe](https://airpipe.io) runs a [shared-nothing architecture](https://en.wikipedia.org/wiki/Shared-nothing_architecture), so our original goal was just to have a simple single binary we could run at our edge and scale HTTP/TCP based containers without the management burden or introducing further additional dependencies to an existing project.

## Feature Highlights

- Written in async Rust for high performance and reliability
- Lightweight - currently <5MB
- Utilizes Cloudflare's [Pingora](https://github.com/cloudflare/pingora/) framework for:
  - Load balancing with health checks
  - Automatic failover
  - High-performance proxying
- Service Discovery:
  - Automatic container registration
  - Dynamic proxy configuration
- Container Management:
  - Automated scaling based on resource usage
  - Rolling updates with configurable strategies
  - Volume management
- Configuration:
  - Simple YAML-based service definitions
  - Hot reload support
  - Flexible resource limits and thresholds
- Monitoring:
  - Prometheus metrics integration
  - Detailed service and container statistics

## Getting Started

### Prerequisites

- Linux x86_64 or aarch64 (other platforms may work but are not officially supported)
    - Mac binary is available in [releases](https://github.com/AirPipeIO/orbit/releases)
- Docker (for now)

### Quick Installation

- Download the latest release
    - https://github.com/AirPipeIO/orbit/releases
- Start Orbit
    - orbit -c /path/to/configs

### Basic Configuration

Create a service configuration file (e.g., `web-service.yml`):

```yaml
name: web-service
instance_count:
  min: 2
  max: 5
spec:
  containers:
    - name: web
      image: airpipeio/infoapp:latest
      ports:
        - port: 80
          node_port: 30080
```

See our [Configuration Reference](docs/configuration.md) for detailed documentation of all available options.

## Documentation

- [Configuration Reference](docs/configuration.md) - Detailed configuration options
- [API Reference](docs/api.md) - API endpoints and usage
- [Examples](examples/) - Various example configurations
- [Contributing](CONTRIBUTING.md) - How to contribute to Orbit

## Contributing

We welcome contributions! Please see our [Contributing Guide](CONTRIBUTING.md) for details on:

- Setting up your development environment
- Our coding standards
- The pull request process
- Running tests

## Community

Discord - [Join our community chat](https://discord.com/invite/b8mFtjWXZj)

## Project Goals

- Maintain simplicity while adding useful features
- Keep resource usage minimal
- Keep small footprint
- Support additional container runtimes
- Implement commonly requested features from for eg. Docker Swarm or Kubernetes

## License

This project is licensed under the Apache License, Version 2.0 - see the [LICENSE](LICENSE) file for details. The Apache License 2.0 provides additional protections for users and contributors, including explicit patent rights grants.