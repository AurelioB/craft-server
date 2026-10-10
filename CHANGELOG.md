# Changelog

All notable changes to Craft Apps Host are documented here. This project follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.1] - 2026-10-10

### Fixed

- OpenID Connect sign-in started on another host name for the server (for example a LAN alias
  behind a local reverse proxy) now continues on the host of `redirect_url`, so the browser-bound
  state and the session cookie land where the provider returns. Previously such a sign-in was
  refused as "not started in this browser".

## [0.1.0] - 2026-10-08

### Added

- A single Rust host that serves the Craft browser apps and launcher, installs releases from their official GitHub repositories, and checks for updates on a schedule.
- Operator commands for configuration and health checks, status, update history, manual updates, pins, rollbacks, and user management.
- An administration page on the apps' port, with optional local-password and OpenID Connect sign-in, user and admin roles, and SQLite-backed accounts.
- Docker Compose deployment with configurable host directories, identity, listener, and optional persistent logs.

### Changed

- App URLs now stay at `/<app>/` when a release changes; retained versions remain available at `/<app>/<version>/`.
- The default retention policy keeps one release per app. Scheduled checks retain their timing across host restarts, and one app's retry no longer triggers checks for every app.
- The host serves app assets and adds precompressed copies to previously installed releases without a separate nginx service.

### Security

- Administration can be restricted to a separate host name; sign-in and proxy identity headers are subject to configured host and trusted-proxy boundaries.
- Container deployment uses a non-root, read-only distroless runtime without privileged mode or a Docker socket.

[0.1.0]: https://github.com/AurelioB/craft-server/releases/tag/v0.1.0
