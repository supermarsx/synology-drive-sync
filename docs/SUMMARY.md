# Summary

[Home](index.md)

# Getting started

- [What it does and what it protects](getting-started/overview.md)
- [Quick start](getting-started/quick-start.md)
- [Installation and deployment](installation.md)

# Configuration

- [Configuration model](configuration/index.md)
- [Complete TOML reference](configuration/reference.md)
- [CLI and environment variables](configuration/cli-and-environment.md)
- [Profiles and precedence](configuration/profiles-and-precedence.md)
- [Passwords, TOTP, and secret sources](configuration/credentials.md)
- [Network, reverse proxy, and TLS](configuration/network.md)
- [Comparison, exclusions, and deletion](configuration/safety.md)
- [Full configuration example](configuration/example.md)
- [Output, logs, and monitoring](observability.md)

# Operations

- [Command reference](reference/cli.md)
- [Diagnostics and multi-profile batches](diagnostics-and-batch.md)
- [Local, mapped-drive, and SMB sources](local-and-smb-sources.md)
- [Scheduling overview](operations/scheduling.md)
  - [Linux systemd](operations/systemd.md)
  - [Portable cron](operations/cron.md)
  - [macOS launchd](operations/launchd.md)
  - [Windows Task Scheduler](operations/windows.md)
  - [Docker and Compose](operations/docker.md)
- [Synology DSM package](synology-package.md)
- [Troubleshooting](operations/troubleshooting.md)
- [Production acceptance and recovery](production-acceptance.md)

# Integration

- [Rust library](sdk/index.md)
  - [Rust quick start](sdk/quick-start.md)
  - [Planning and execution](sdk/workflows.md)
  - [Cancellation, progress, and errors](sdk/progress-and-errors.md)
  - [Generated Rust API](sdk/api-reference.md)
- [C ABI, DLL, and shared objects](ffi/index.md)
  - [ABI surface](ffi/api.md)
  - [Ownership, errors, and threading](ffi/memory-errors-and-threading.md)
  - [FFI examples](ffi/examples.md)
  - [Distribution and compatibility](ffi/distribution.md)

# Release and assurance

- [Release selector](release-selector.md)
- [Release artifacts and verification](releases.md)
- [Testing and coverage](testing.md)
- [Security](security.md)
- [Contributing](contributing.md)
- [License and third-party notices](legal.md)
