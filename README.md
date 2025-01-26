# Orbit

## What is Orbit

Orbit is a simple, light weight solution to scaling containers with inbuilt service discovery and proxying.

## Why?
- Docker swarm has no autoscaling, Kubernetes has a large learning and management overhead. 
- [Air Pipe](https://airpipe.io) runs a [shared-nothing architecture](https://en.wikipedia.org/wiki/Shared-nothing_architecture), so our original goal was just to have a simple single binary we could run at our edge and scale HTTP/TCP based containers without the management burden or introducing further additional dependencies to an existing project.

## Feature highlights

- Async Rust
- Utilizes Cloudflare's [Pingora](https://github.com/cloudflare/pingora/) rust framework for load balancing, failover and proxying
- Hot reload
- Automatic service discovery
- Simple scaling
- Simple config definition
- Prometheus metrics

## Getting Started
See our quick start guide here, and examples.

## System Requirements
- Linux is the main focus, so x84_64 and aarch64 will be the main releases.
- Anything else is community best effort.

## Project Goals

- Keep it simple
- Other container runtimes
- Any useful missing features from swarm, or K8s