# Craft Apps Host: Project Plan

Date: October 8, 2026

Status: Design only. This document does not authorize implementation or deployment.

## 1. Objective

Create a standalone Docker Compose project that serves the official Craft browser apps and updates their downloaded releases without rebuilding container images or restarting the web server for each app update.

Priorities:

- Prompt updates directly from official releases.
- Continued availability when updates fail or the internet is unavailable.
- Configurable directories, process identities, permissions, and update policies.
- Reproducible installation, observable status, and straightforward rollback.
- Independence from any particular NAS, reverse proxy, authentication provider, or container manager.

## 2. Applications And Filesystem Boundaries

Initially support PhotoCraft, VectorCraft, FilmCraft, LightCraft, PdfCraft, EffectCraft, and DesignCraft. Each app can be enabled or disabled independently.

These browser applications execute on the visitor's computer, not in the hosting container. The server supplies the HTML, JavaScript, WebAssembly, and other application assets.

Opening a local file normally loads it into the browser; it does not inherently upload it to the hosting server. Depending on the application and browser, saving can use a download, a user-approved local file or folder, or browser-managed storage.

Consequently:

- Container UID/GID settings govern server processes and files they create, not documents saved on a visitor's computer.
- Mounting a server folder into the web container does not make it available in the apps' file dialogs.
- The hosting configuration cannot choose browser-side working directories or grant access to local folders automatically.
- An SMB share mounted on the visitor's computer can be used through the ordinary file picker where the app and browser support it.
- Server-side file browsing, shared project storage, synchronization, and native desktop remote access are separate projects.

The host will not claim to provide server-side document storage or shared workspaces.

## 3. Architecture

Use two independently restartable services in one Compose project:

| Service | Responsibilities | Storage access |
| --- | --- | --- |
| Web server | Serve the launcher, installed app releases, and non-sensitive status | Read-only access to published content |
| Updater | Discover releases, download, validate, install, retain history, and handle operator commands | Write access to its designated data, state, cache, and work directories |

Use a standard unprivileged Nginx runtime and a standard Python runtime with project code mounted read-only. Avoid package installation during container startup; design the updater around the Python standard library where practical.

App updates change persistent files, not container images. Updating the server runtime or updater code remains a separate maintenance operation.

Neither service requires a Docker socket, privileged mode, GPU passthrough, a database, or an AI generation backend. The updater has no publicly exposed management port.

## 4. Project Deliverables

The standalone repository will contain:

```text
compose.yaml
.env.example
config.example.toml
server/
launcher/
updater/
tests/
README.md
docs/
```

Deliver a launcher, updater, operator CLI, configuration validation, health checks, automated tests, and documentation for installation, permissions, updates, retention, backup, and recovery.

Provide configuration examples rather than hardcoded environment-specific directories or identities.

## 5. Configurable Directories

Clearly distinguish host bind-mount sources from container paths. Host paths are configurable; internal mount destinations may remain stable to keep the Compose configuration simple.

| Setting | Purpose |
| --- | --- |
| `CONFIG_DIR` | Operator-managed configuration, mounted read-only |
| `DATA_DIR` | Immutable installed releases, published launcher/status, and current-version pointers |
| `STATE_DIR` | Installed-version records, locks, pins, blocked versions, and update history |
| `CACHE_DIR` | Downloaded release archives and reusable download cache |
| `WORK_DIR` | Temporary downloads, extraction, and validation workspace |
| `LOG_DIR` | Optional persistent logs; standard container logs remain the default |
| `WEB_WORKING_DIR` | Working directory of the web-server process inside its container |
| `UPDATER_WORKING_DIR` | Working directory of the updater process inside its container |

A relative host path is resolved consistently from the Compose project; production examples favor explicit absolute paths. Support spaces in paths without shell-string concatenation.

Allow per-app release subdirectory configuration under `DATA_DIR`. Validate that paths cannot escape their permitted roots or overlap destructively. Do not require a document workspace mount because the browser apps cannot use it directly.

Downloads and preliminary validation may use a separate filesystem. Final staging and release promotion must happen on the same filesystem as the installed releases to preserve atomic publication.

Validate existence, ownership, readability, writability, available space, and atomic-promotion requirements before installation. Explain failures without changing unrelated directories.

## 6. User, Group, And Permission Configuration

Use actual process identities through Compose `user`, not unused environment variables that merely resemble identity controls.

Common settings:

```dotenv
RUN_UID=1000
RUN_GID=10000
FILE_UMASK=0002
CONFIG_DIR=/srv/craft-apps/config
DATA_DIR=/srv/craft-apps/data
STATE_DIR=/srv/craft-apps/state
CACHE_DIR=/srv/craft-apps/cache
WORK_DIR=/srv/craft-apps/work
```

These are example values, not installation-specific requirements.

Support optional `WEB_UID`, `WEB_GID`, `UPDATER_UID`, and `UPDATER_GID` overrides, plus explicit supplemental groups when needed. Validate numeric IDs and make non-root operation the default. Document host user-namespace and Docker Desktop differences where ownership does not map directly.

Requirements:

- Every server-side write uses the configured process identity and umask.
- The web process can read published content but cannot modify it.
- Temporary files and persistent logs follow the same ownership policy.
- Preserve setgid inheritance when the operator configures it.
- Do not restore unsafe archive ownership, setuid/setgid bits, or executable permissions unnecessarily.
- Never recursively change ownership of existing bind mounts automatically.
- Provide a permission-check command with actionable preparation instructions.
- Protect credentials separately from publicly served files.

Using different service identities requires explicitly compatible group permissions; the configuration checker must detect incompatibility.

## 7. Application And Update Configuration

Maintain an explicit application manifest with:

- App identifier and display name.
- Official repository and accepted web-archive filename patterns.
- Enabled state and stable browser entry path.
- Optional pinned version and automatic-update flag.
- Release channel, retention policy, and documented serving requirements.

Handle repository and artifact names separately. For example, the `printcraft` repository currently publishes `pdfcraft` web artifacts; do not assume names always match.

Default to official published non-prerelease releases. Development-branch builds and source compilation are outside the default update path. Configurable prerelease support must be explicit.

Default behavior: check on startup and hourly afterward, with configurable interval, timeouts, bounded retries, and backoff. Respect API rate limits. An optional GitHub credential is supplied through a protected file and never printed.

## 8. Safe Update Lifecycle

For each enabled, unpinned app:

1. Discover an eligible official release and identify its web artifact.
2. Confirm it is not already installed or blocked after a rollback.
3. Check download and extraction size budgets and available disk space.
4. Download into temporary storage with bounded timeouts and retries.
5. Verify the published checksum; skip releases without adequate integrity metadata.
6. Safely extract into an unpublished staging directory.
7. Reject path traversal, unsafe links, device entries, excessive file counts, oversized expansion, and unexpected layouts.
8. Validate the entry page, referenced files, WebAssembly payloads, MIME handling, compression, and path-prefix compatibility.
9. Test candidate serving through a private validation route unavailable from the public listener.
10. Publish an immutable release directory and atomically replace the active-version pointer.
11. Record the installed version and update outcome, then apply retention rules.

Use a shared lock to serialize automatic and manual operations. State files are written atomically and reconciled with published releases after a crash. A crash must leave either the previous complete release or the new complete release active, never a partially extracted app.

A failed update must not affect the working release. Each app updates independently, so one failure does not block the rest.

Checksum validation detects corruption or mismatched downloads; it is not proof that upstream code is secure or bug-free.

## 9. Web Serving And Open Tabs

Provide a simple launcher with app names, icons, and installed versions. Use stable entry paths such as `/photocraft/` and `/pdfcraft/`.

Prefer redirecting stable entry paths to immutable, version-specific release URLs. This lets already-open tabs continue requesting matching JavaScript and WebAssembly assets after an update.

Verify each app's actual base-path, service-worker, and asset-resolution behavior. Do not assume all archives work under nested paths. If an incompatible release layout cannot be handled safely using documented configuration, skip it and report the reason rather than silently patching the app.

Configure correct MIME types, compression, and cache headers. Versioned immutable assets may be cached long-term; launcher/status content and current-version redirects must revalidate. Do not serve secrets, updater state, download caches, staging files, or detailed diagnostics publicly.

Keep each app's browser hostname and entry identity stable. Test storage namespaces for conflicts when multiple apps share one origin; support separate-host serving if testing shows it is required.

## 10. Retention, Pins, And Rollback

Default retention keeps the latest three installed releases and every release installed within the previous 30 days. Always retain the active release regardless of policy. Configure cache and log limits separately.

Provide operator commands for:

- `status` and `doctor`.
- `check` and `update`, for one app or all enabled apps.
- `pin` and `unpin`.
- `rollback`, using a retained release.
- Explicitly allowing a previously blocked release.

These are planned CLI capabilities, not commands already implemented.

Rolling back blocks automatic reinstallation of that same release until the operator allows it or a newer eligible release appears. Test rollback behavior rather than assuming changing a pointer is sufficient.

Browser-side data migrations may not be reversible. Rolling back the hosted files does not guarantee recovery of a browser library. Documents and browser libraries require their own export/backup strategy.

Very old tabs may need refreshing after their release is removed by retention. Document this limit.

## 11. Status, Health, And Failure Handling

Publish non-sensitive status: installed/latest versions, pinned state, last check, last successful update, and concise failure summaries.

Maintain separate health signals:

- Web-server liveness.
- Availability/readiness of enabled apps.
- Updater heartbeat.
- Update freshness and individual app failures.

An updater outage must not mark an otherwise working web server unavailable. On first startup, explain which apps are still being installed. Offline restarts serve existing releases immediately without waiting for GitHub.

Test and document handling of API outages, rate limits, missing artifacts, checksum failures, incompatible layouts, insufficient disk space, permission failures, interrupted downloads, and process crashes. Failed releases remain visible in status; errors are not silently ignored.

## 12. Deployment Boundary

The project serves HTTP behind an operator-selected HTTPS/authentication proxy. HTTPS is important for browser capabilities and must be documented as a deployment requirement outside localhost.

Do not include NAS-specific paths, Komodo resources, Dockflare labels, Authentik providers, or mandatory hostnames in the core project.

The later personal deployment preference is Local + Dockflare. Authentication and routing will be configured separately, without exposing the updater or providing an unauthenticated bypass.

Browser storage is origin-specific. Different LAN and public hostnames do not automatically share libraries, even when they point to the same server.

## 13. Verification And Acceptance

Automated tests cover configuration, permissions, archive safety, checksum handling, naming changes, update locks, atomic promotion, restart recovery, retention, pins, and rollback.

Integration tests cover configurable paths including spaces, different UID/GID combinations, read-only serving, failed/offline startup, simultaneous update attempts, interrupted downloads, and disk exhaustion.

Browser tests cover all seven enabled apps: nonblank rendering, assets loading, opening a representative file, editing and saving where supported, storage isolation, and an open tab during an update. Check relevant desktop and mobile viewports while documenting app/browser limitations.

Packaging and serving checks are not a guarantee that every upstream editing function works. Record test coverage and failures honestly.

Acceptance criteria:

- App updates require neither an image rebuild nor a web-server restart.
- Update failures preserve complete working releases.
- Existing apps remain available offline.
- Every server-side write uses the configured UID/GID and permission policy.
- All required server paths are configurable and validated.
- Public serving cannot expose configuration, credentials, caches, or updater internals.
- Version status, pinning, and rollback are usable and documented.
- No server-side document access or synchronization is implied.

## 14. Implementation Order

1. Verify current official artifacts and serving requirements for all seven apps.
2. Implement configuration, identity/permission checks, and persistent layout.
3. Implement download verification, safe extraction, state reconciliation, and atomic installation.
4. Add serving, launcher, version status, CLI, and retention/rollback.
5. Add automated tests and browser verification.
6. Complete operator documentation and a reproducible local demonstration.
7. Separately plan the NAS deployment after the standalone project is accepted.

## References

- [Official Craft apps overview](https://getartcraft.com/apps)
- [Example official web releases: PhotoCraft](https://github.com/storytold/photocraft/releases)
- [Community packaging reference: ArtCraft in a Box](https://github.com/sbuchweitz/artcraft-in-a-box)
- [GitHub Releases API](https://docs.github.com/en/rest/releases/releases)
- [Docker Compose service configuration](https://docs.docker.com/reference/compose-file/services/)
- [Browser File System API](https://developer.mozilla.org/en-US/docs/Web/API/File_System_API)
