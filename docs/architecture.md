# Architecture and design decisions

## Components

| Path | Contents |
| --- | --- |
| `compose.yaml`, `compose.logs.yaml` | Services, identities, mounts, health checks; optional log mount |
| `server/` | nginx configuration (`nginx.conf`, `mime.types`, `headers.conf`), mounted read-only |
| `launcher/` | Static launcher page and its script, mounted read-only |
| `updater/` | Rust crate `craft-updater` (daemon + CLI) and its `Dockerfile` |
| `updater/src/manifest.toml` | Built-in application manifest, compiled into the binary |
| `tests/integration/` | Compose override used by the Docker integration test |

Updater modules: `config` (manifest + `config.toml` + environment, validation), `github`
(release discovery, ETag cache, bounded transfers, rate limits), `archive` (zip inspection and
extraction), `validate` (content checks, serving check), `layout` (directory layout, promotion,
symlinks, reconciliation), `ops` (update lifecycle, pins, rollback, retention), `store` (state,
history, heartbeat, lock), `status`, `doctor`, `daemon`.

## Publication model

```mermaid
sequenceDiagram
  participant U as updater
  participant FS as DATA_DIR
  participant W as nginx
  U->>FS: extract to .staging/<app>-<v>-<rand>/site
  U->>W: GET :8081/candidate/<staging>/site/… (MIME, gzip, references)
  U->>FS: write .craft-release.json
  U->>FS: rename site → releases/<app>/<v> (atomic)
  U->>FS: symlink .current.tmp → <v>; rename over current (atomic)
  W->>FS: per request: realpath(public/<app>/current) → redirect to <v>/
```

- A release directory either does not exist or is complete: it appears through one `rename(2)`
  inside `DATA_DIR`. The `current` pointer is replaced with `rename(2)` of a new symlink.
- nginx resolves `public/<entry>/current` on every request (`$realpath_root`, no open-file cache)
  and answers `/<entry>/` with a relative `302` to `<version>/`. Nothing is reloaded or restarted.
- Release URLs are immutable, so assets are cached for a year; entry pages, `sw.js`, manifests
  and redirects revalidate.
- The updater writes `.craft-release.json` into each release so state can be rebuilt from disk.
  Dotfiles and the `current` alias are never served.
- `public/` contains only `status.json` and the per-app entry symlinks. State, cache, staging,
  configuration and credentials are outside the web server's document root or not mounted at all.

## Decisions and deviations from the plan

- **Updater in Rust instead of Python.** The plan called for a standard Python runtime with the
  updater code mounted read-only. At the owner's request the updater is written in Rust, like the
  Craft apps. It is built once into a small image (static musl binary on
  `gcr.io/distroless/static-debian12`), so updating the updater is an image rebuild; app updates
  still need neither an image rebuild nor a web-server restart. Dependencies are pinned by
  `Cargo.lock`; TLS uses rustls with bundled WebPKI roots, so the runtime image needs no CA store.
- **Serving check through nginx.** Candidates are fetched through a second nginx listener (8081)
  that serves `DATA_DIR/.staging` with the same MIME and compression settings. It is reachable only
  on the Compose network. When it is unreachable, the update is deferred rather than skipped.
- **LightCraft exclusions.** The manifest's `exclude` list removes the Cargo build directory that
  LightCraft 0.4.0 ships inside its web archive. Native executables anywhere else cause rejection.
  No application file is modified.
- **Separate origins** are a link-level option (`origin`), because the web server already answers
  on any host name and all apps work under a path prefix.
- **No document workspace or server-side storage**, as planned.
