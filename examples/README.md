# Orbit Example Configurations

This directory contains example configurations demonstrating various Orbit features. All examples use our test container `airpipeio/infoapp:latest` unless specific features require different images.

Always refer to [configuration](/docs/configuration.md) for the latest configuration options as our focus is to iterate quickly. We will do best effort to keep the examples up to date.

## Basic Examples

- [helloworld.yml](configs/helloworld.yml) - Simple single container service with basic resource limits
- [helloworld-sidecar.yml](configs/helloworld-sidecar.yml) - Multi-container service with nginx sidecar

## Resource Management

- [resource-limits.yml](configs/resource-limits.yml) - CPU, memory and network rate limiting with thresholds

## Scaling

- [codel-scaling.yml](configs/codel-scaling.yml) - Latency-based adaptive scaling with CoDel

## Health Monitoring

- [health-checks.yml](configs/health-checks.yml) - TCP health checks and zero-downtime updates

## Test Application Features

The `airpipeio/infoapp:latest` container used in these examples provides several testing endpoints:

- `/` - Shows container info (hostname, IPs)
- `/sleep?duration=1000` - Simulates CPU load
- `/?size=1048576` - Returns large response (1MB)
- `/tc?action=add&delay=50` - Adds network delay (requires privileged mode)

Each configuration file is thoroughly documented with comments explaining the settings and their effects. For full configuration options, see our [Configuration Reference](../docs/configuration.md).