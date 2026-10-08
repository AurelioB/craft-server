# Operations

All commands run in the updater container:

```sh
docker compose exec updater craft-updater <command>
```

Manual commands take the same lock as the daemon. If an update is running they wait up to
`lock_wait` (default 10 minutes) and then fail with "another update operation holds the lock".

## How updates work

On startup and every `check_interval` (default 1 hour) the daemon, for each enabled app:

1. Lists recent releases (conditional request: unchanged lists cost no API quota).
2. Picks the newest release that is not a draft, not a pre-release (GitHub flag *or* a semver
   suffix such as `-rc.5`), and has exactly one asset matching the app's artifact patterns.
3. Skips it if it is not newer than the active release, is blocked by a rollback, or the app is
   pinned or has `auto_update = false`.
4. Checks the download against `max_download` and free space in `WORK_DIR`.
5. Downloads with timeouts and bounded retries, hashing as it goes.
6. Compares the SHA-256 with `SHA256SUMS.txt` from the release and the digest GitHub reports for
   the asset. Both, if present, must agree; at least one must exist or the release is skipped.
7. Inspects the zip: no traversal, absolute paths, links, devices or encrypted entries; entry
   count, expanded size and compression ratio within limits; `index.html` present. Manifest
   exclusions drop known non-web content (LightCraft's Cargo build directory).
8. Extracts into `DATA_DIR/.staging`, then validates: entry page, every referenced file exists, no
   root-absolute URLs (path-prefix compatibility), WebAssembly magic, no native executables,
   precompressed `.gz` copies identical to their originals.
9. Fetches the candidate through the web server's private listener: entry page as `text/html`,
   `.wasm` as `application/wasm` and compressed, `.js` as `text/javascript`, references reachable.
10. Writes a release marker, renames the directory into `<release_dir>/<version>` (one atomic
    rename), makes it read-only, and atomically replaces the `current` symlink.
11. Records the result and applies retention.

Each app is processed independently; one failing app does not hold back the others. After a
failure the app waits (1, 2, 4 … minutes, at most `check_interval`) or until GitHub's rate-limit
reset; `update` bypasses the wait.

Checksum verification detects corrupted or substituted downloads. It does not prove that
upstream code is secure or bug-free.

## Status and health

| Signal | Where | Meaning |
| --- | --- | --- |
| Web liveness | `GET /healthz`, Compose health check of `web` | nginx is serving |
| App readiness | `GET /readyz/<app>` | 200 once a release is active, 503 before |
| Updater heartbeat | Compose health check of `updater`; `status` | daemon loop alive (stale after 2 min) |
| Freshness and failures | `status`, `/status.json`, launcher | last check, last update, latest seen, errors |

`/status.json` is public and non-sensitive: versions, pin/block state, timestamps and short
error summaries with container paths replaced by directory names. Disabled apps are omitted.
`status --json` shows the full detail. An updater outage never makes the web server unhealthy;
the launcher shows a notice when the heartbeat is older than 10 minutes.

On the very first start the launcher lists apps as "being installed" and their entry paths serve
a self-refreshing page with HTTP 503 until the first release is published.

## Pins

```sh
craft-updater pin photocraft 0.3.0     # installs 0.3.0 if needed, activates it, stops updates
craft-updater unpin photocraft         # next update moves to the newest eligible release
```

A pin can also be set in `config.toml` (`pinned_version`); a CLI pin takes precedence, and
`unpin` reports when a configuration pin remains. A pinned release may be a pre-release. An
installed pin is activated even while GitHub is unreachable.

## Rollback and blocked releases

```sh
craft-updater rollback photocraft          # newest retained release older than the active one
craft-updater rollback photocraft 0.3.0    # a specific retained release
craft-updater allow photocraft 0.5.0       # permit the rolled-back-from release again
```

Rollback switches the `current` pointer to a retained release and blocks the release that was
active, so automatic updates do not reinstall it. The block ends when you `allow` it (the next
update reactivates it without downloading) or when a newer eligible release is published.

Rolling back hosted files does not roll back data in visitors' browsers. Apps may migrate their
browser storage on first use of a newer version, and an older version may not read it.
Documents and browser libraries need their own export or backup from inside each app.

## Retention

Per app the updater keeps the active release, a pinned release, the `keep_latest` most recently
installed releases (default 3) and every release installed within `keep_days` (default 30).
Other releases are removed after each successful update. Removal is atomic per release: the
directory is moved into `.staging` first, then deleted.

Tabs opened on a release that retention has since removed fail to load further files from it;
reloading the app's stable URL (`/photocraft/`) moves them to the active release.

Downloaded archives in `CACHE_DIR/archives` are kept for reuse (reinstalling, failed
installs that are retried) and pruned least-recently-used first above `cache_max_size`.

## Enabling, disabling and adding apps

`enabled = false` in `[apps.<id>]` removes the app's public entry link (its stable URL returns
404) and stops updates; its releases and state stay on disk. Additional apps can be defined in
`config.toml` with `name`, `repository` and `artifact_patterns` (see `config.example.toml`).

## Separate origins

All apps share one browser origin by default. Each app namespaces most of its browser storage,
but some use generic names (see [apps](apps.md#browser-storage)). To give an app its own
storage, serve it under its own host name, which is a separate origin:

1. Point the extra host name (for example `lightcraft.example.net`) at the same proxy and web
   service; the web server answers on any host name.
2. Set `origin = "https://lightcraft.example.net"` in `[apps.lightcraft]`.

The launcher then links to `https://lightcraft.example.net/lightcraft/`. The app remains
reachable under the main host name as well; this setting changes links, not routing, so restrict
it in the proxy if the shared origin must not be used.

## Backup

| Directory | Back up? | Notes |
| --- | --- | --- |
| `CONFIG_DIR` | yes | configuration and token (protect the backup) |
| `STATE_DIR` | yes | pins, blocks, history; small |
| `DATA_DIR` | optional | releases can be re-downloaded, but old versions may disappear upstream; back up to keep the ability to roll back |
| `CACHE_DIR`, `WORK_DIR` | no | reproducible |

Back up while no update is running (`status` shows `idle`), or stop the updater first; the web
server can keep running.

## Recovery

- **Crash or power loss during an update.** On start the updater removes `.staging` and partial
  downloads and reconciles state with the `current` symlinks, which only ever name complete
  releases. Running `update` performs the same reconciliation.
- **Lost or corrupt state.** Release directories carry `.craft-release.json` markers; delete the
  app's file in `STATE_DIR/apps/` and run `update` to rebuild the record from disk. Pins and
  blocks have to be set again.
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
| Validation listener down | retried with backoff | `serve-check`: listener unreachable |
| Not enough disk space | checked before download and extraction | `space` error |
| Interrupted download | retried, partial file removed | `download` error if retries run out |
| Permission problem | operation fails, nothing changed | error mentions `doctor` |
| Process crash | reconciliation on next start | `history` |

Failed releases stay listed in `status` until a later release installs successfully.
