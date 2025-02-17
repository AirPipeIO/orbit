# Orbit Configuration Reference

This document provides a comprehensive reference for Orbit's configuration options. For practical examples and quick-start configurations, see our [Examples Directory](examples/).

## Basic Configuration Structure

Service configurations are YAML files containing:
- Service metadata and scaling parameters
- Resource limits and thresholds
- Container specifications
- Optional volume and network configurations

## Configuration Reference

### Core Service Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | Yes | Service name (must be DNS label compatible) |
| `instance_count` | object | Yes | Scaling boundaries (min/max) |
| `cpu_limit` | string/number | No | Service-level CPU limit (e.g., "1.0" = 1 core) |
| `memory_limit` | string/number | No | Service-level memory limit (e.g., "512M", "1G") |
| `resource_thresholds` | object | No | Scaling thresholds |
| `network` | string | No | Custom network name |
| `volumes` | object | No | Volume definitions |
| `scaling_policy` | object | No | Scaling behavior configuration |
| `codel` | object | No | CoDel-based autoscaling configuration |

### Resource Thresholds

Control when scaling actions are triggered:

```yaml
resource_thresholds:
  cpu_percentage: 80            # CPU usage threshold
  cpu_percentage_relative: 90   # CPU usage relative to limit
  memory_percentage: 85         # Memory usage threshold
  metrics_strategy: max         # How to aggregate pod metrics (max/average)
```

### Advanced Scaling Configuration

#### CoDel-based (Controlled Delay) Autoscaling

See https://en.wikipedia.org/wiki/CoDel for more information.

Adaptive scaling based on request latency:

```yaml
codel:
  target: 100ms                # Target latency threshold
  interval: 1s                 # Check interval
  consecutive_intervals: 3      # Intervals above target before scaling
  max_scale_step: 1            # Max instances to add at once
  scale_cooldown: 30s          # Time between scaling actions
  overload_status_code: 503    # Response code when overloaded
```

#### Scaling Policy

Fine-tune scaling behavior:

```yaml
scaling_policy:
  cooldown_duration: 60s
  scale_down_threshold_percentage: 50.0
```

### Container Configuration

Detailed container settings:

```yaml
spec:
  containers:
    - name: app
      image: airpipeio/infoapp:latest
      command: ["/app/server"]      # Optional command override
      ports:
        - port: 80                  # Container port
          target_port: 8080         # Optional host port mapping
          node_port: 30080          # Optional external access port
      memory_limit: "1G"            # Container-specific limit
      cpu_limit: "0.5"             # Container-specific CPU limit
      network_limit:                # Optional rate limiting
        ingress_rate: "10Mbps"
        egress_rate: "5Mbps"
      health_check:                 # Health monitoring
        tcp_check:
          port: 80
          timeout: 5s
      volume_mounts:                # Optional volume mounts
        - name: config
          mount_path: /etc/config
          read_only: true
```

### Volume Configuration

Different volume types:

```yaml
volumes:
  config-files:                    # Inline files
    files:
      "config.json": |
        {"key": "value"}
  
  persistent-data:                 # Named volume
    named_volume:
      name: "myapp-data"
      labels:
        environment: "prod"
  
  host-mount:                      # Host path mounting
    host_path: "/path/on/host"
    permissions: "0644"
```

### Rolling Updates

Configure update behavior:

```yaml
rolling_update_config:
  max_unavailable: 1              # Max pods down during update
  max_surge: 1                    # Max extra pods during update
  timeout: 5m                     # Update timeout
```

## Testing Your Configuration

The `airpipeio/infoapp:latest` image provides several endpoints for testing:

- `/` - Basic container info
- `/sleep?duration=1000` - Simulate CPU load
- `/?size=1048576` - Return large response (1MB)
- `/tc?action=add&delay=50` - Add network delay (requires privileged mode)

## Additional Examples

For more complex configurations and real-world examples, check our [examples directory](examples/configs):