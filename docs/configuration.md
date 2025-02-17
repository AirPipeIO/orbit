# Orbit Configuration Reference

This document provides a comprehensive reference for Orbit's configuration options. For practical examples and quick-start configurations, see our [Examples Directory](examples/).

## Basic Configuration Structure

Service configurations are YAML files containing:
- Service metadata and scaling parameters
- Resource limits and thresholds
- Container specifications
- Optional volume and network configurations

## Core Service Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | Yes | Service name (must be a valid DNS label: lowercase alphanumeric characters or '-', starting and ending with alphanumeric) |
| `network` | string | No | Name of network to use for containers. If not specified, a dedicated network is created for multi-container pods |
| `pull_policy` | string | No | Global image pull policy for all containers ('Always' or 'Never'). 'Always' ensures latest image is pulled on every container start, 'Never' uses cached image. Defaults to 'Never'. Can be overridden per container. |
| `adopt_orphans` | boolean | No | Whether to adopt existing containers that match the service pattern (default: false) |
| `instance_count` | object | Yes | Defines scaling boundaries |
| `memory_limit` | string/number | No | Service-level memory limit (e.g., "2Gi", "512Mi") |
| `cpu_limit` | string/number | No | Service-level CPU limit (e.g., "1.0" = 1 core) |
| `image_check_interval` | duration | No | Interval for checking container image updates |
| `rolling_update_config` | object | No | Configuration for rolling updates |
| `resource_thresholds` | object | No | Resource thresholds for autoscaling |
| `volumes` | object | No | Named volume definitions |
| `codel` | object | No | CoDel-based adaptive scaling configuration |
| `scaling_policy` | object | No | General scaling policy configuration |

### Instance Count Configuration

```yaml
instance_count:
  min: 1  # Minimum number of instances to maintain
  max: 5  # Maximum number of instances allowed
```

### Resource Thresholds

```yaml
resource_thresholds:
  cpu_percentage: 80         # CPU usage threshold (percentage)
  cpu_percentage_relative: 90 # CPU usage relative to limit
  memory_percentage: 85      # Memory usage threshold
  metrics_strategy: max      # Strategy for pod metrics (max/average)
```

### CoDel-based (Controlled Delay) Autoscaling
See https://en.wikipedia.org/wiki/CoDel for more information.

Adaptive scaling based on request latency:

```yaml
codel:
  target: 100ms           # Target latency threshold
  interval: 1s            # Interval for checking delays
  consecutive_intervals: 3 # Intervals above target before scaling
  max_scale_step: 1       # Maximum instances to scale up at once
  scale_cooldown: 30s     # Minimum time between scaling actions
  overload_status_code: 503 # Status code to return when overloaded
```

### Scaling Policy
Fine-tune scaling behavior:


```yaml
scaling_policy:
  cooldown_duration: 60s     # Time between scaling actions
  scale_down_threshold_percentage: 50.0  # CPU/Memory threshold for scale down
```

### Rolling Update Configuration

```yaml
rolling_update_config:
  max_unavailable: 1    # Maximum pods that can be unavailable during update
  max_surge: 1         # Maximum extra pods that can be created during update
  timeout: 5m         # Timeout for update process
```

## Container Configuration

Each service defines one or more containers under the `spec.containers` field:

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Container name (must be DNS label compatible) |
| `image` | string | Container image to use |
| `command` | array | Optional command override |
| `privileged` | boolean | Run container in privileged mode. Required for advanced network operations (e.g., traffic shaping) and certain system-level access. Use with caution as it grants elevated permissions. Default: false |
| `pull_policy` | string | Container-specific image pull policy ('Always' or 'Never'). Overrides service-level setting. 'Always' pulls latest image on start, 'Never' uses cached. Default: 'Never' |
| `ports` | array | Port configurations |
| `volume_mounts` | array | Volume mount configurations |
| `memory_limit` | string/number | Container-specific memory limit |
| `cpu_limit` | string/number | Container-specific CPU limit |
| `network_limit` | object | Network rate limiting configuration |
| `health_check` | object | Health check configuration |
| `resource_thresholds` | object | Container-specific resource thresholds |

### Port Configuration

```yaml
ports:
  - port: 80          # Container port
    target_port: 8080 # Host port mapping (optional)
    node_port: 30080  # External access port (optional)
    protocol: TCP     # Protocol (TCP/UDP)
```

### Network Limit Configuration

```yaml
network_limit:
  ingress_rate: "10Mbps"    # Incoming traffic limit
  egress_rate: "5Mbps"      # Outgoing traffic limit
```

### Health Check Configuration

```yaml
health_check:
  startup_timeout: 30s
  startup_failure_threshold: 3
  liveness_period: 10s
  liveness_failure_threshold: 3
  tcp_check:
    port: 80
    timeout: 5s
    period: 10s
    failure_threshold: 3
```

### Volume Configuration

```yaml
volumes:
  config:                     # Volume name
    files:                    # Inline files
      "config.json": |
        {"key": "value"}
    host_path: "/host/path"  # Optional host path
    permissions: "0644"      # Optional file permissions
    named_volume:            # Optional named volume
      name: "volume-name"
      labels:
        environment: "prod"
```

Each configuration file should be thoroughly tested before deployment. For working examples of these configurations, see our [Examples Directory](examples/).

## Testing Your Configuration

The `airpipeio/infoapp:latest` image provides several endpoints for testing:

- `/` - Basic container info
- `/sleep?duration=1000` - Simulate CPU load
- `/?size=1048576` - Return large response (1MB)
- `/tc?action=add&delay=50` - Add network delay (requires privileged mode)

## Additional Examples

For more complex configurations and real-world examples, check our [examples directory](examples/configs):