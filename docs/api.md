# Orbit API Reference

> **Note**: The API is under active development and will be expanded with additional endpoints and features.

Orbit provides a HTTP API for monitoring and managing services. The API server runs on port 4112 by default.

## Endpoints
- /status
- /metrics

### Status API

#### Get Service Status

```http
GET /status
```

Returns the current status of all services including their containers, ports, health information, and resource usage statistics.

**Response Format:**
```json
[
  {
    "service_name": "string",
    "service_ports": [
      number
    ],
    "service_urls": [
      {
        "url": "string",
        "node_port": number
      }
    ],
    "pods": [
      {
        "uuid": "string",
        "containers": [
          {
            "name": "string",
            "ip_address": "string",
            "ports": [
              {
                "port": number,
                "target_port": number,
                "node_port": number,
                "healthy": boolean
              }
            ],
            "status": "string",
            "cpu_percentage": number,
            "cpu_percentage_relative": number,
            "memory_usage": number,
            "memory_limit": number,
            "urls": [
              {
                "url": "string",
                "port": number,
                "target_url": "string",
                "target_port": number
              }
            ],
            "health_status": {
              "state": "string",
              "last_state": "string",
              "last_transition": "string",
              "restart_count": number,
              "last_restart": "string",
              "failure_count": number,
              "last_failure": "string",
              "message": "string"
            }
          }
        ]
      }
    ]
  }
]
```

**Example Response:**
```json
[
  {
    "service_name": "web-service",
    "service_ports": [80],
    "service_urls": [
      {
        "url": "http://localhost:30080",
        "node_port": 30080
      }
    ],
    "pods": [
      {
        "uuid": "550e8400-e29b-41d4-a716-446655440000",
        "containers": [
          {
            "name": "web-service__0__nginx__550e8400",
            "ip_address": "172.17.0.2",
            "ports": [
              {
                "port": 80,
                "target_port": null,
                "node_port": 30080,
                "healthy": true
              }
            ],
            "status": "running",
            "cpu_percentage": 0.5,
            "cpu_percentage_relative": 25.0,
            "memory_usage": 52428800,
            "memory_limit": 268435456,
            "urls": [
              {
                "url": "http://172.17.0.2:80",
                "port": 80,
                "target_url": null,
                "target_port": null
              }
            ],
            "health_status": {
              "state": "Healthy",
              "last_state": "Starting",
              "last_transition": "2024-02-17T10:30:00Z",
              "restart_count": 0,
              "failure_count": 0,
              "message": null
            }
          }
        ]
      }
    ]
  }
]
```

### Metrics API

#### Get Prometheus Metrics

```http
GET /metrics
```

Returns metrics in Prometheus exposition format, including:

**Service-level Metrics:**
- `orbit_services_total` - Total number of services being managed
- `orbit_instances_total` - Total number of container instances
- `orbit_config_reloads_total` - Total number of configuration reloads

**Request Metrics:**
- `orbit_service_request_duration_seconds` - Request duration histograms per service
- `orbit_service_requests_total` - Total requests per service
- `orbit_requests_total` - Total number of requests processed

**Volume Metrics:**
- `orbit_volumes_total` - Total number of named volumes
- `orbit_volume_usage_bytes` - Volume usage in bytes
- `orbit_volume_containers` - Number of containers using each volume

**Example Response:**
```
# HELP orbit_services_total Total number of services being managed
# TYPE orbit_services_total gauge
orbit_services_total 2

# HELP orbit_instances_total Total number of container instances
# TYPE orbit_instances_total gauge
orbit_instances_total 5

# HELP orbit_service_request_duration_seconds Request duration in seconds per service
# TYPE orbit_service_request_duration_seconds histogram
orbit_service_request_duration_seconds_bucket{service="web-service",status="200",le="0.005"} 1
...

# HELP orbit_volume_usage_bytes Volume usage in bytes
# TYPE orbit_volume_usage_bytes gauge
orbit_volume_usage_bytes{volume="data"} 1048576
```

Please check back regularly as we continue to expand the API functionality. For the latest updates, see our [GitHub repository](https://github.com/airpipeio/orbit).