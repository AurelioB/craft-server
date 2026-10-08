# Architecture and design decisions

## Components

| Path | Contents |
| --- | --- |
| `compose.yaml`, `compose.logs.yaml` | The `host` service: identity, mounts, health check; optional log mount |
| `host/` | Rust crate `craft-host` (server, updater, admin, CLI) and its `Dockerfile` |
| `host/assets/` | Launcher, admin and login pages, compiled into the binary |
| `host/src/manifest.toml` | Built-in application manifest, compiled into the binary |
| `tests/integration/` | Compose override used by the Docker integration test |

Modules of `craft-host`:

| Module | Role |
| --- | --- |
| `server`, `serve` | HTTP routing, launcher, `/status.json`, health, app paths, static file serving |
| `login`, `auth`, `oidc`, `access` | sign-in (`/auth/`), roles, apps gate, users file, sessions, CSRF, proxy identities, OIDC, settings |
| `admin`, `assets` | `/admin` pages and API; embedded launcher assets (logos, fonts) |
| `daemon`, `activity` | process start, background updater and heartbeat threads, idle activation, request activity |
| `ops` | update lifecycle, activation policy, pins, rollback, retention |
| `github`, `archive`, `validate`, `precompress`, `candidate` | discovery and transfer, zip safety, content checks, `.br`/`.gz` copies, private serving check |
| `layout`, `store`, `config`, `status`, `doctor` | data layout and reconciliation, state and lock, configuration, status, diagnostics |

One process: the HTTP server runs on a Tokio runtime; the updater, the heartbeat and admin
actions run on their own threads and never block serving.

## Publication model

```mermaid
sequenceDiagram
  participant U as updater thread
  participant FS as DATA_DIR
  participant C as candidate listener (127.0.0.1)
  participant S as HTTP server
  U->>FS: extract to .staging/<app>-<v>-<rand>/site, validate, add .br/.gz
  U->>C: GET /candidate/<staging>/site/… (MIME, compression, references)
  U->>FS: write .craft-release.json; rename site → releases/<app>/<v> (atomic)
  alt immediate, or idle long enough
    U->>FS: symlink .current.tmp → <v>; rename over current (atomic)
  else app recently requested
    U->>FS: record <v> as pending; activate later
  end
  S->>FS: per request: read current → 302 <v>/; serve releases/<app>/<v>/…
```

- A release directory either does not exist or is complete: it appears through one `rename(2)`
  inside `DATA_DIR`. The `current` pointer is replaced with `rename(2)` of a new symlink and read
  on every request to the stable path, so activation needs no restart or reload.
- Release URLs are immutable, so assets are cached for a year; entry pages, `sw.js`, manifests
  and redirects revalidate.
- The serving check uses the production file-serving code (`serve::serve_dir`) on a loopback
  listener that serves only `.staging`, so a candidate is tested exactly as it will be served
  without ever being reachable from outside.
- Only release files are served. Paths are percent-decoded and refused if any segment is hidden
  (`.craft-release.json`, `.htaccess`, `..`) or contains a backslash or NUL; the version segment
  must name a directory with a release marker; `current` is never a valid version.

## Decisions and deviations from the plan

- **One Rust binary instead of nginx plus a Python updater.** The plan called for two independently
  restartable services, a standard nginx runtime with read-only content and a Python updater. At
  the owner's request the project now ships one image whose binary serves, updates and
  administers, written in Rust like the Craft apps. Consequences:
  - the serving process can write releases and read the GitHub token and admin secrets; the
    path checks above, a read-only root filesystem, dropped capabilities and a non-root identity
    are the boundary;
  - an updater fault cannot restart the server (the health check only probes `/healthz`), but
    updating the binary restarts serving briefly;
  - static serving, precompression and caching are implemented in the binary (tower-http's
    `ServeDir`) instead of nginx configuration.
- **Activation policy.** The plan asked to apply updates "when the app is not being used". The
  server only observes requests, so `idle` activation is a request-inactivity heuristic;
  versioned URLs make immediate activation safe for open tabs, which is why it is the default.
- **One port, roles.** Launcher, apps, sign-in and `/admin` share one listener. `[auth]` signs
  people in for the whole site; the admin role manages updates, and `[auth] apps = "signed-in"`
  optionally limits the apps to accounts with the user or admin role. `/admin` therefore shares
  the apps' origin unless `[admin] host` gives it a host name of its own; the trade-off (app code
  could use an administrator's session) is documented in [admin.md](admin.md).
- **LightCraft exclusions.** The manifest's `exclude` list removes the Cargo build directory that
  LightCraft 0.4.0 ships inside its web archive. Native executables anywhere else cause rejection.
  No application file is modified; precompressed copies are added alongside.
- **Separate origins** are a link-level option (`origin`), because the server answers on any host
  name and all apps work under a path prefix.
- **No document workspace or server-side storage**, as planned.
