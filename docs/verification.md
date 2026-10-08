# Verification record

Recorded 8 October 2026 on Linux (Docker 29.8, Compose 5.6, Rust 1.99), against the official
releases listed in [apps.md](apps.md). Packaging and serving checks do not guarantee that every
upstream editing function works.

## Automated tests

`cd updater && cargo test` (unit and lifecycle tests) and
`cargo test --test compose -- --ignored` (Docker Compose integration).

| Suite | Count | Covers |
| --- | --- | --- |
| Unit (`src/`) | 32 | configuration validation (escaping/overlapping paths, reserved entries, unknown keys, units, origins), umask parsing, read-only trees keeping setgid, atomic symlink swap, version precedence, artifact matching across renames, pre-release detection, SHA256SUMS parsing, archive safety (traversal, absolute names, links, FIFOs, size/count/ratio limits, case collisions, file/dir conflicts, missing entry page), no exec/setuid bits on extraction, content validation (root-absolute and missing references, native binaries, invalid wasm, mismatched `.gz`), compression threshold kept in sync with `nginx.conf`, lock exclusion, state round-trip |
| Lifecycle (`tests/lifecycle.rs`, fake GitHub) | 12 | install/update with atomic publication; checksum mismatch, disagreeing sources and missing metadata; unsafe and incompatible archives; interrupted downloads with bounded retries; rate limits and backoff; GitHub 5xx; disk-space budget; renamed artifacts; rollback/block/allow/supersede; CLI and config pins incl. offline; retention; crash reconciliation; concurrent CLI processes; umask on every written file |
| Compose (`tests/compose.rs`) | 1 scenario | host paths with spaces; real nginx serving check; relative redirects; `current` and dotfiles hidden; read-only web mount; state/config not mounted in web; update without web restart while the old release keeps serving; two simultaneous `update` commands (one install); disk exhaustion on a 1 MiB `WORK_DIR`; offline restart serving immediately; `doctor` detecting an incompatible web identity under umask 0027 and accepting it with a supplemental group; serving denied/allowed accordingly |

All suites passed. The Compose test needs `server/` and `launcher/` readable by the foreign test
identity (see [permissions](permissions.md)).

## Live deployment against GitHub

Stack started with directories under a path containing a space, `RUN_UID=1000`,
`RUN_GID=10000`, `FILE_UMASK=0002`, setgid directories.

- All seven apps installed from GitHub, each passing the serving check through nginx. LightCraft:
  549 Cargo build entries excluded by the manifest.
- First start with the web service unavailable: installs deferred with backoff
  (`serve-check: validation listener unreachable`), then completed once it was up.
- Responses: `/photocraft/` → `302 0.5.0/` (query string preserved, `no-cache`); `.wasm`
  `application/wasm` + gzip + `immutable`; `.js` `text/javascript`; `sw.js`, manifests and entry
  pages `no-cache`; `/<app>/current/…`, `.craft-release.json`, `.htaccess`, excluded `deps/`,
  `/.staging/` → 404; uninstalled app → 503 "being installed" page; port 8081 not published.
- Files on the host: owner `1000`, group `10000` inherited via setgid, releases `0444`/`2555`,
  status `0664`; web container could not write to `/srv/data` or its root filesystem.
- `pin photocraft 0.3.0` installed and activated the older release; `unpin` + `update`
  reactivated 0.5.0 without downloading.
- `doctor`: all checks passed for this layout.

## Browser checks (headless Chromium, `http://localhost:18080`)

| App | Renders | Failed requests / page errors | File opened | Edited / saved |
| --- | --- | --- | --- | --- |
| PhotoCraft 0.5.0 | yes (WebGL2 backend) | none | PNG via file picker | inverted, saved (PSD download, 542 KB) |
| VectorCraft 0.7.0 | yes | none | SVG via drag and drop | not exercised |
| FilmCraft 0.4.0 | yes, demo project plays in program monitor | none | not exercised | not exercised |
| LightCraft 0.4.0 | yes, demo library and develop panel | none | not exercised | not exercised |
| PdfCraft 0.4.0 | yes | none | PDF via file picker, page rendered | not exercised |
| EffectCraft 0.6.0 | yes, demo composition renders; service worker registered for `/effectcraft/0.6.0/` | none | not exercised | not exercised |
| DesignCraft 0.4.0 | yes | none | not achieved: synthetic clicks on "Open"/"Open sample magazine" did not register in headless mode | not exercised |

- **Open tab during an update.** A PhotoCraft tab loaded from `/photocraft/0.3.0/` kept working
  after 0.5.0 was activated: it created a new document and re-fetched its JS and wasm (200 from
  the 0.3.0 path); navigating to `/photocraft/` then opened 0.5.0.
- **Storage.** All apps on one origin used distinct names in the observed session (listed in
  [apps.md](apps.md#browser-storage)); `localhost:18080` and `127.0.0.1:18080` had fully separate
  storage, confirming that a separate host name isolates an app. The `origin` setting produced
  absolute launcher links for the configured app.
- **Viewports.** Launcher checked at 1280×800 and 390×844 (single column). LightCraft at
  390×844 renders its desktop layout with overlapping header controls; the apps are designed for
  desktop-size windows (upstream limitation).

## Not verified

- Behaviour on real HTTPS behind a reverse proxy, WebGPU rendering, and browsers other than
  Chromium.
- Saving in apps other than PhotoCraft; app-specific import formats beyond PNG, SVG and PDF.
- User-namespace remapping and Docker Desktop.
- Hourly scheduling over long periods (exercised with short intervals and manual commands).
