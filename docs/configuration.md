# Orbit Configuration Reference

This document details all available configuration options for Orbit services.

## Service Configuration Structure

Service configurations are defined in YAML files and support the following top-level fields:

### Core Service Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | Yes | Service name (must be a valid DNS label: lowercase alphanumeric characters or '-', starting and ending with alphanumeric) |
| `network` | string | No | Name of network to use for containers. If not specified, a dedicated network is created for multi-container pods |
| `adopt_orphans` | boolean | No | Whether to adopt existing containers that match the service pattern (default: false) |
| `instance_count` | object | Yes | Defines scaling boundaries |
| `memory_limit` | string/number | No | Service-level memory limit (e.g., "2Gi", "512Mi") |
| `cpu_limit` | string/number | No | Service-level CPU limit (e.g., "1.0" = 1 core) |
| `image_check_interval` | duration | No | Interval for checking container image updates |
| `rolling_update_config` | object | No | Configuration for rolling updates |
| `resource_thresholds` | object | No | Resource thresholds for autoscaling |
| `volumes` | object | No | Named volume definitions |

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

### Rolling Update Configuration

```yaml
rolling_update_config:
  max_unavailable: 1    # Maximum pods that can be unavailable during update
  max_surge: 1         # Maximum extra pods that can be created during update
  timeout: 5m         # Timeout for update process
```

### Container Configuration

Each service defines one or more containers under the `spec.containers` field:

```yaml
spec:
  containers:
    - name: app
      image: nginx:latest
      command: ["/bin/sh", "-c", "nginx"]
      ports:
        - port: 80
          target_port: 8080
          node_port: 30080
      memory_limit: "1Gi"
      cpu_limit: "0.5"
      network_limit:
        ingress_rate: "10Mbps"
        egress_rate: "5Mbps"
      volume_mounts:
        - name: config
          mount_path: /etc/nginx/conf.d
          read_only: true
```

#### Container Fields

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Container name (must be DNS label compatible) |
| `image` | string | Container image to use |
| `command` | array | Optional command override |
| `ports` | array | Port configurations |
| `volume_mounts` | array | Volume mount configurations |
| `memory_limit` | string/number | Container-specific memory limit |
| `cpu_limit` | string/number | Container-specific CPU limit |
| `network_limit` | object | Network rate limiting configuration |

### Port Configuration

```yaml
ports:
  - port: 80          # Container port
    target_port: 8080 # Host port mapping (optional)
    node_port: 30080  # External access port (optional) (this enables the pingora proxy for the service)
    protocol: TCP     # Protocol (TCP/UDP)
```

### Volume Configuration

```yaml
volumes:
  config:
    files:
      "nginx.conf": |
        server {
          listen 80;
          location / {
            proxy_pass http://service-containername;
          }
        }
    host_path: "/path/on/host"  # Optional host path mounting
    permissions: "0644"         # Optional file permissions
    named_volume:              # Optional named volume configuration
      name: "nginx-config"
      labels:
        environment: "prod"
```

## Example Configurations

### Simple Web Service

```yaml
name: web-service
instance_count:
  min: 2
  max: 5
spec:
  containers:
    - name: mywebservice
      image: airpipeio/infoapp:latest
      ports:
        - port: 80
          node_port: 30080
```

### Multi-Container Service with Resource Limits

```yaml
name: backend-service
instance_count:
  min: 1
  max: 3
memory_limit: "4Gi"
cpu_limit: "2.0"
resource_thresholds:
  cpu_percentage: 80
  memory_percentage: 85
spec:
  containers:
    - name: app
      image: my-app:latest
      ports:
        - port: 8080
          node_port: 30088
      memory_limit: "2Gi"
      cpu_limit: "1.0"
    - name: cache
      image: redis:latest
      ports:
        - port: 6379
      memory_limit: "1Gi"
```

### Service with Volume Mounts

```yaml
name: database
instance_count:
  min: 1
  max: 1
volumes:
  data:
    named_volume:
      name: "db-data"
      labels:
        type: "persistent"
spec:
  containers:
    - name: postgres
      image: postgres:13
      ports:
        - port: 5432
      volume_mounts:
        - name: data
          mount_path: /var/lib/postgresql/data
```