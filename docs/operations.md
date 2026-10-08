# Operations

Manage updates from `/admin` ([admin.md](admin.md)) or the command line:

```sh
docker compose exec host craft-host <command>
```

Both take the same lock as scheduled updates. A manual command waits up to `lock_wait` (default
10 minutes) for a running update and then fails with "another update operation holds the lock";
`/admin` refuses a second action while one runs.

## How updates work

On startup and every `check_interval` (default 1 hour) the background updater, for each enabled
app:

1. Lists recent releases (conditional request: unchanged lists cost no API quota).
2. Picks the newest release that is not a draft, not a pre-release (GitHub flag *or* a semver
   suffix such as `-rc.5`), and has exactly one asset matching the app's artifact patterns.
3. Skips it if it is not newer than the active release, is blocked by a rollback, or the app is
   pinned or has `auto_update = false`.
4. Checks the download against `max_download` and free space in `WORK_DIR`, downloads with
   timeouts and bounded retries, and compares the SHA-256 with `SHA256SUMS.txt` from the release
   and the digest GitHub reports for the asset. Both, if present, must agree; at least one must
   exist or the release is skipped.
5. Inspects the zip: no traversal, absolute paths, links, devices or encrypted entries; entry
   count, expanded size and compression ratio within limits; `index.html` present. Manifest
   exclusions drop known non-web content (LightCraft's Cargo build directory).
6. Extracts into `DATA_DIR/.staging`, then validates: entry page, every referenced file exists, no
   root-absolute URLs (path-prefix compatibility), WebAssembly magic, no native executables,
   shipped `.gz`/`.br` copies identical to their originals.
7. Writes missing `.br` and `.gz` copies of compressible files (≥ 1 KiB), so large WebAssembly
   modules are never compressed per request.
8. Fetches the candidate over HTTP through the same serving code, on a private loopback listener:
   entry page as `text/html`, `.wasm` as `application/wasm` and compressed, `.js` as
   `text/javascript`, references reachable.
9. Writes a release marker, renames the directory into `<release_dir>/<version>` (one atomic
   rename) and makes it read-only.
10. Activates it (atomically replaces the `current` symlink) or leaves it **pending**, see below.
11. Records the result and applies retention.

Each app is processed independently. After a failure the app waits (1, 2, 4 … minutes, at most
`check_interval`) or until GitHub's rate-limit reset; manual updates bypass the wait.

Checksum verification detects corrupted or substituted downloads. It does not prove that
upstream code is secure or bug-free.

## Activation: immediate or idle

`activation` (global under `[updater]`, or per app) decides when a downloaded release becomes the
one new visitors get:

- `immediate` (default): as soon as it is installed.
- `idle`: once the app has received **no requests for `idle_after`** (default 30 minutes). Until
  then the release is *pending*: shown in `/admin`, `status` and the launcher, and activated by a
  check that runs every minute.

Either way, open tabs keep working: a tab that loaded `/photocraft/0.5.0/` keeps requesting files
from that versioned path, which stays published.

Idle detection only sees requests. The apps run in the browser and may make no requests for hours
while someone works in them, so "idle" means "nobody has opened or reloaded the app recently",
not "nobody has it open". After a restart, idle time counts from the start. Manual updates, pins
and "Apply" activate immediately; a rollback or pin discards a pending release. A command-line
`update` without the server's request data never assumes an app is idle.

## Status and health

| Signal | Where | Meaning |
| --- | --- | --- |
| Liveness | `GET /healthz`, Compose health check | the HTTP server answers |
| App readiness | `GET /readyz/<app>` | 200 once a release is active, 503 before |
| Updater heartbeat | `/admin`, `status`, launcher notice | background updater alive |
| Freshness and failures | `/admin`, `status`, `/status.json` | last check, last update, latest, pending, errors |

`/status.json` is public and non-sensitive: versions, pin/block/pending state, timestamps and
short error summaries with container paths replaced by directory names. Disabled apps are omitted.
A stalled updater never fails the health check, so it cannot cause a working server to restart.

On the very first start the launcher lists apps as "being installed" and their entry paths serve
a self-refreshing page with HTTP 503 until the first release is published.

## Pins

Pin in `/admin` or with `craft-host pin photocraft 0.3.0` (installs 0.3.0 if needed, activates
it, stops updates); `unpin` returns to the newest eligible release. A pin can also be set in
`config.toml` (`pinned_version`); an interface/CLI pin takes precedence, and `unpin` reports when a
configuration pin remains. An installed pin is activated even while GitHub is unreachable.

## Rollback and blocked releases

Roll back in `/admin` or with `craft-host rollback photocraft [VERSION]` (default: the newest
retained release older than the active one). The release that was active is blocked, so updates
do not reinstall it, until you allow it (`allow`; the next update reactivates it without
downloading) or a newer eligible release is published. A pending release is discarded.

Rolling back hosted files does not roll back data in visitors' browsers. Apps may migrate their
browser storage on first use of a newer version, and an older version may not read it.
Documents and browser libraries need their own export or backup from inside each app.

## Retention

Per app the updater keeps the active and pending releases, a pinned release, the `keep_latest`
most recently installed releases (default 3), every release installed within `keep_days`
(default 30), and every release that served requests within `keep_recently_used` (default 24 h,
known only since the last restart). Others are removed after each successful update; removal
moves the directory into `.staging` first, then deletes it.

Tabs opened on a release that retention has since removed fail to load further files from it;
reloading the app's stable URL (`/photocraft/`) moves them to the active release.

Downloaded archives in `CACHE_DIR/archives` are kept for reuse and pruned least-recently-used
first above `cache_max_size`.

## Enabling, disabling and adding apps

`enabled = false` in `[apps.<id>]` removes the app from the launcher, makes its URLs answer 404
and stops its updates; releases and state stay on disk. Additional apps can be defined in
`config.toml` with `name`, `repository` and `artifact_patterns` (see `config.example.toml`).
Configuration changes take effect after `docker compose restart host`.

## Separate origins for apps

All apps share one browser origin by default. Each app namespaces most of its browser storage,
but some use generic names (see [apps](apps.md#browser-storage)). To give an app its own
storage, serve it under its own host name, which is a separate origin:

1. Point the extra host name (for example `lightcraft.example.net`) at the same proxy; the server
   answers on any host name.
2. Set `origin = "https://lightcraft.example.net"` in `[apps.lightcraft]`.

The launcher then links to `https://lightcraft.example.net/lightcraft/`. This changes links, not
routing; restrict the shared host in the proxy if the app must not be used there.

## Backup

| Directory | Back up? | Notes |
| --- | --- | --- |
| `CONFIG_DIR` | yes | configuration, admin users, token, OIDC secret (protect the backup) |
| `STATE_DIR` | yes | pins, blocks, pending, history; small |
| `DATA_DIR` | optional | releases can be re-downloaded, but old versions may disappear upstream; back up to keep the ability to roll back |
| `CACHE_DIR`, `WORK_DIR` | no | reproducible |

Back up while no update is running (`status` shows the heartbeat phase `idle`), or stop the
container first.

## Recovery

- **Crash or power loss during an update.** On start the server removes `.staging` and partial
  downloads and reconciles state with the `current` symlinks, which only ever name complete
  releases. `update` performs the same reconciliation.
- **Lost or corrupt state.** Release directories carry `.craft-release.json` markers; delete the
  app's file in `STATE_DIR/apps/` and run `update` to rebuild the record from disk. Pins, blocks
  and pending activations have to be set again. A corrupt record shows as `state` failure in
  status instead of being treated as empty.
- **Release directory without a marker** (not created by the updater). Left untouched and not
  served; remove it by hand.
- **Restore from backup.** Restore `CONFIG_DIR`, `STATE_DIR` and (optionally) `DATA_DIR`, then
  start the stack; reconciliation drops records of releases that are not on disk.

## Failure handling

| Situation | Behaviour | Visible as |
| --- | --- | --- |
| GitHub unreachable / HTTP 5xx | bounded retries, then retry with backoff | `discover` error; apps keep serving |
| Rate limit (403/429) | waits until GitHub's reset time | `discover`: rate limit; configure a token |
| No matching artifact / ambiguous artifacts | release skipped | `status --json`: `skipped` notes |
| Checksum mismatch or missing | release skipped, retried later | `checksum` error, `failed_versions` |
| Unsafe archive / unexpected layout | release rejected | `archive` or `validate` error |
| Serving check fails | release rejected | `serve-check` error |
| Not enough disk space | checked before download and extraction | `space` error |
| Interrupted download | retried, partial file removed | `download` error if retries run out |
| Permission problem | operation fails, nothing changed | error mentions `doctor` |
| Process crash | reconciliation on next start | `history` |

Failed releases stay listed until a later release installs successfully.
