# Configuration reference

Craft Apps Host combines its compiled-in [app manifest](../host/src/manifest.toml), an optional operator `config.toml`, and process environment variables. Copy the [annotated example](../config.example.toml) to `CONFIG_DIR/config.toml` to start; it contains the default settings and examples of optional sections. The file is read when the process starts: restart the server after changing it. Commands invoked separately read it again. A missing file uses built-in defaults; an unreadable or invalid file is an error. Unknown TOML keys are rejected, including unknown keys in app tables; built-in apps also reject changes to their definitions.

`CRAFT_CONFIG` selects the config file (default `/config/config.toml`). `CONFIG_DIR` in this document means **the directory containing that file**, not an independently read environment variable; relative GitHub token and OIDC secret paths resolve there. In the supplied Compose setup, the host-side `.env` variable `CONFIG_DIR` is mounted read-only at `/config`. The other `.env` path variables are host-side mount sources, not process settings. Use separate absolute container paths for config, data, state, cache and work (and logs, if enabled); `LOG_DIR` may instead be a child of `STATE_DIR`. Other overlapping paths are rejected.

**Values and units:** A size takes a nonnegative integer number of bytes or a quoted nonnegative number with an optional case-insensitive unit: `B`, `KB`, `MB`, `GB`, `TB` (powers of 1000) or `KiB`, `MiB`, `GiB`, `TiB` (powers of 1024). Without a unit, a string is bytes. A duration takes a nonnegative integer number of seconds or a quoted nonnegative number with optional *lowercase* `s`, `m`, `h` or `d` (seconds, minutes, hours, days); without a unit, a string is seconds. Boolean settings use TOML `true`/`false`; lists use TOML arrays. Where no minimum is listed, zero is accepted. Integer fields marked `u32`/`u64` must fit the corresponding unsigned type; sizes/durations use the parser's saturating arithmetic on overflow.

## `[server]`

| Key | Type; default | Meaning and validation |
| --- | --- | --- |
| `listen` | string; `"0.0.0.0:8080"` | TCP socket address (`IP:port`; bracket IPv6 literals), on the container's network. Compose publishes container port 8080. |
| `trusted_proxies` | string list; `[]` | IPv4/IPv6 addresses or CIDR networks whose forwarding and proxy-auth headers are trusted. Bare addresses match one host; CIDR prefixes must fit the address family. Only trusted peers' `X-Forwarded-Proto` (HTTPS/Secure-cookie detection), `X-Forwarded-Host` and configured identity headers are honored. Required nonempty for proxy sign-in. Set this to the actual proxy network, not arbitrary clients. |

## `[updater]`

| Key | Type; default | Meaning and validation |
| --- | --- | --- |
| `check_on_startup` | bool; `true` | Continue the persisted check schedule: apps never checked or already due are checked promptly. `false` waits a full interval after startup. |
| `check_interval` | duration; `"1h"` (3600s) | Interval for scheduled checks; at least 60s. |
| `http_timeout` | duration; `"30s"` | GitHub API request timeout (also used by doctor/OIDC connectivity); greater than zero. |
| `download_timeout` | duration; `"15m"` | Archive-download request timeout; greater than zero. |
| `max_retries` | u32; `3` | Additional attempts for retryable network requests; at most 10. |
| `retry_backoff` | duration; `"5s"` | Initial retry delay. |
| `retry_backoff_max` | duration; `"5m"` | Maximum retry delay. |
| `github_api_url` | string; `"https://api.github.com"` | Base URL of GitHub-compatible API; must start with `http://` or `https://`. |
| `github_token_file` | string; `""` | Optional readable token file for GitHub API authentication. Empty disables it; relative paths resolve against `CONFIG_DIR`, absolute paths are accepted. The token is not logged. |
| `activation` | string; `"immediate"` | `"immediate"` makes new installations active for new visitors at once; `"idle"` waits for the app to receive no requests for `idle_after`. Existing tabs retain their release URLs, but idle detection cannot see activity that generates no requests. Per-app override available. |
| `idle_after` | duration; `"30m"` | Quiet period for idle activation; per-app override available. |
| `heartbeat_interval` | duration; `"30s"` | Interval for updater heartbeat writes; greater than zero. |
| `lock_wait` | duration; `"10m"` | Time operations wait for the shared updater lock. |
| `log_max_size` | size; `"10MiB"` | Rotate `LOG_DIR/updater.log` after a logged line takes it over this size; used only when file logging is enabled. |
| `log_backups` | u32; `5` | Rotated log files to keep (`updater.log.1`, etc.); `0` discards the old file on rotation. |

## `[limits]`

| Key | Type; default | Meaning and validation |
| --- | --- | --- |
| `max_download` | size; `"512MiB"` | Maximum download archive size. |
| `max_extracted` | size; `"1GiB"` | Maximum total extracted size of an archive. |
| `max_files` | u64; `5000` | Maximum extracted file count; greater than zero. |
| `max_compression_ratio` | u64; `200` | Maximum archive compression ratio; greater than zero. |
| `min_free_space` | size; `"256MiB"` | Minimum free space reserved when downloading to `WORK_DIR`, caching in `CACHE_DIR`, and promoting to `DATA_DIR`. |

## `[retention]`

| Key | Type; default | Meaning and validation |
| --- | --- | --- |
| `keep_latest` | u32; `1` | Installed releases kept per app, **counting the active release first**; at least 1. Set 2 or more to normally retain an older release for rollback. Per-app override available (there is no additional per-app minimum check). Pending and pinned releases are also retained. |
| `keep_days` | u32; `0` | Also retain releases installed within this many days; `0` disables the age rule. Per-app override available. |
| `keep_recently_used` | duration; `"0"` (0s) | Also retain replaced releases that served requests recently so existing tabs can fetch release-specific files; `0` disables the grace period. Request activity is in memory, so it does not survive restart. |
| `cache_max_size` | size; `"2GiB"` | Maximum total size of downloaded archives; prune least-recently-used archives. Does not include the API response cache. |

## `[auth]`

Accounts and roles (`user`, `admin`) live in `STATE_DIR/users.sqlite3` and are managed through `/admin` or `craft-host user …`. Configure sign-in independently of app access. Administrators can also use apps.

| Key | Type; default | Meaning and validation |
| --- | --- | --- |
| `methods` | string list; `[]` | Allowed entries: `"local"` (password form), `"oidc"` (OpenID Connect), `"basic"` (HTTP Basic against the account database), `"proxy"` (trusted proxy identity). Empty means no sign-in: anyone reaching enabled `/admin` is an administrator; only use behind external protection. `local` and `oidc` can be combined; `basic` and `proxy` each must stand alone. OIDC needs `[auth.oidc]`; proxy needs `[server] trusted_proxies`. |
| `apps` | string; `"public"` | `"public"`: anyone who reaches the server can access launcher and apps. `"signed-in"`: only signed-in users/admins; requires a nonempty sign-in method. |
| `session_ttl` | duration; `"12h"` | Interactive sign-in session lifetime; at least `"1m"`. |
| `cookie_secure` | string; `"auto"` | `"auto"` sets Secure when the client used HTTPS (including trusted `X-Forwarded-Proto`), `"always"` always sets it, `"never"` never sets it. |
| `admin_users` | string list; `[]` | OIDC/proxy user names granted admin. With proxy sign-in and enabled admin, this list or `admin_groups` must be nonempty. |
| `admin_groups` | string list; `[]` | OIDC/proxy groups granted admin. With proxy sign-in and enabled admin, this list or `admin_users` must be nonempty. |
| `user_groups` | string list; `[]` | OIDC/proxy groups granted user access; empty admits other authenticated identities as users. Admin matches take precedence. If this list is nonempty, identities matching neither an admin list nor a user group have no access. |

For OIDC, setting **any** of the three role lists makes provider claims decide roles at every sign-in; with all three empty, account roles are managed locally and new OIDC accounts receive the user role. For proxy authentication, the proxy supplies identity and groups; secure the proxy and do not expose a path around it.

### `[auth.oidc]`

Only required if `"oidc"` is in `methods`. The section's options have the defaults below; `issuer`, `client_id` and `redirect_url` must be supplied for working OIDC sign-in.

| Key | Type; default | Meaning and validation |
| --- | --- | --- |
| `name` | string; `"single sign-on"` | Label on the sign-in button. |
| `issuer` | string; `""` | OIDC issuer URL; trimmed of trailing `/`; must start with `http://` or `https://` when OIDC is enabled. |
| `client_id` | string; `""` | OIDC client identifier; nonempty when OIDC is enabled. |
| `client_secret_file` | string; `"oidc-client-secret"` | Secret file path relative to `CONFIG_DIR`, or absolute. A missing file enables a public PKCE client; protect a present secret file from other users. |
| `redirect_url` | string; `""` | Public callback URL; must start with `http://` or `https://` and end in `/auth/oidc/callback` when OIDC is enabled. |
| `scopes` | string list; `["openid", "profile", "email"]` | Requested OIDC scopes; must include `"openid"` when OIDC is enabled. |
| `username_claim` | string; `"preferred_username"` | Provider claim used as the account name. |
| `groups_claim` | string; `"groups"` | Provider claim used for group-based role rules. |
| `create_users` | bool; `true` | Create a local account for a previously unknown OIDC identity on first sign-in. |
| `link_by_email` | bool; `false` | Link unknown identities to existing accounts with the same email only when the provider reports that email verified. Enable only if the provider does not reassign email addresses. |

OIDC sign-in cannot be combined with `[admin] host`: its single callback host cannot establish a session on the separate admin host.

### `[auth.proxy]`

Used with `methods = ["proxy"]`. Header names must be nonempty and contain only ASCII letters, digits, `-` or `_`. Both are accepted **only** from trusted proxy peers.

| Key | Type; default | Meaning and validation |
| --- | --- | --- |
| `user_header` | string; `"Remote-User"` | Header containing the signed-in user name. |
| `groups_header` | string; `"Remote-Groups"` | Header containing groups (comma- or `|`-separated). |

## `[admin]`

| Key | Type; default | Meaning and validation |
| --- | --- | --- |
| `enabled` | bool; `false` | Serve the administration interface at `/admin/` on the apps' listener. With no sign-in configured, anyone who can reach it is an administrator. |
| `host` | string; `""` (unset) | Restrict `/admin` to this host name (optional `:port`); apps are not served on that host. Whitespace is trimmed and the host is lowercased. Nonempty values require nonempty dot-separated ASCII alphanumeric/`-` labels and, if specified, a nonzero numeric port fitting `u16`. No URL scheme/path or IPv6 literal. Incompatible with OIDC sign-in. Without a separate host, admin and apps share a browser origin. |

## Apps and the built-in manifest

The [built-in manifest](../host/src/manifest.toml) is compiled into `craft-host`; do not edit it to customize a deployed server. By default all seven apps are enabled, auto-updated on the stable channel, use `SHA256SUMS.txt` as a checksum-asset name, and serve under `/<entry>/` (stable URL) while versioned resources live at `/<entry>/<version>/`. An app's default `entry` is its ID; its default `release_dir` is `releases/<id>` under `DATA_DIR`. Each of these manifest apps has its own repository and ZIP artifact pattern(s):

| ID / entry | Repository | ZIP artifact patterns | Manifest notes |
| --- | --- | --- | --- |
| `photocraft` | `storytold/photocraft` | `photocraft-web-{version}.zip` | Content-hashed assets; WebGPU requires a secure context, with WebGL2 fallback. |
| `vectorcraft` | `storytold/vectorcraft` | `vectorcraft-web-{version}.zip` | Content-hashed assets; WebGPU requires a secure context, with WebGL2 fallback. |
| `filmcraft` | `storytold/filmcraft` | `filmcraft-web-{version}.zip` | Projects use origin-scoped browser storage (OPFS); `icon = "favicon.png"`. |
| `lightcraft` | `storytold/lightcraft` | `lightcraft-web-{version}.zip` | Experimental; origin-scoped browser library, one tab at a time, precompressed `.gz` copies. Manifest excludes unused native-build files (`.fingerprint/**`, `build/**`, `deps/**`, `examples/**`, `incremental/**`, `.cargo-lock`, `.cargo-build-lock`, `.cargo-artifact-lock`). |
| `pdfcraft` | `storytold/printcraft` | `pdfcraft-web-{version}.zip`, `printcraft-web-{version}.zip` | Older artifacts were named `printcraft-web` (through v0.2.1); renamed `pdfcraft-web` from v0.4.0. |
| `effectcraft` | `storytold/effectcraft` | `effectcraft-web-{version}.zip` | Release-scoped service worker; projects use origin-scoped browser storage; `icon = "favicon.svg"`. |
| `designcraft` | `storytold/designcraft` | `designcraft-web-{version}.zip` | Content-hashed assets; WebGPU requires a secure context, with WebGL2 fallback. |

Manifest definitions also include each app's `name`, `category`, `tagline` and `color` for the launcher. A `[apps.<id>]` section with an existing ID overlays **only** operator settings; omitting it leaves all manifest/default values intact. Built-in apps allow exactly: `enabled`, `auto_update`, `pinned_version`, `channel`, `release_dir`, `entry`, `origin`, `activation`, `idle_after`, `keep_latest`, `keep_days`. A definition key (`name`, `repository`, `artifact_patterns`, `checksum_assets`, `exclude`, `icon`, `notes`, `category`, `tagline`, `color`) is rejected in a built-in app section even if its value equals the manifest value.

An additional `[apps.<id>]` accepts **all** keys below and must define `name`, `repository` and a nonempty `artifact_patterns` list; it inherits the other defaults shown. IDs (and `entry`) are 1–63 ASCII bytes, start with a lowercase letter or digit, and otherwise contain only lowercase letters, digits or `-`. Entries must be unique; `admin`, `auth`, `launcher`, `healthz`, `readyz` and `status` are reserved. Release directories may not overlap, even for disabled apps.

### `[apps.<id>]` operator settings (built-in or additional)

| Key | Type; default | Meaning and validation |
| --- | --- | --- |
| `enabled` | bool; `true` | Whether the app is included in serving and scheduled updates. |
| `auto_update` | bool; `true` | If `false`, allow initial installation but skip later automatic upgrades; manual `update` can still install. |
| `pinned_version` | string; unset (`""` is also unset) | Pin to a semantic release version (e.g. `"0.5.0"`, optionally prefixed with `v`). Trims whitespace; a CLI/admin-set pin in persistent state takes precedence. |
| `channel` | string; `"stable"` | `"stable"` excludes prereleases; `"prerelease"` also considers prereleases. |
| `release_dir` | string; `"releases/<id>"` | App-specific relative path under `DATA_DIR`; no absolute path, backslash, parent traversal or component beginning with `.`. Each normalized component must start with an ASCII letter or digit; remaining characters may be ASCII letters, digits, `.`, `_`, `-` or space. Must not overlap another app's release directory. |
| `entry` | string; `<id>` | Unique app URL path segment (`/<entry>/`), subject to the ID syntax and reserved-entry rules above. |
| `keep_latest` | u32; `[retention] keep_latest` (`1` by default) | Override the per-app count of retained releases, active first. The global setting checks for at least 1; the per-app override does not have that validation. |
| `keep_days` | u32; `[retention] keep_days` (`0` by default) | Override per-app age-based retention in days. |
| `origin` | string; unset (`""` is also unset) | Optional separate app link origin for isolated browser storage. `http(s)://host[:port]` only, no path or IPv6 literal; host DNS-style labels (1–63 ASCII letters/digits/`-`, not beginning/ending in `-`; total host length at most 253), port 1–65535. Whitespace and trailing `/` are trimmed. The server answers on that host; DNS/TLS and routing are your responsibility. |
| `activation` | string; `[updater] activation` (`"immediate"` by default) | Override when this app's new release becomes active: `"immediate"` or `"idle"`. |
| `idle_after` | duration; `[updater] idle_after` (`"30m"` by default) | Override quiet period for this app's idle activation. |

### `[apps.<id>]` definition keys (additional apps only)

| Key | Type; default | Meaning and validation |
| --- | --- | --- |
| `name` | string; **required** | App's displayed name; no further nonempty check. |
| `repository` | string; **required** | GitHub `owner/name`; each side must be nonempty and contain only ASCII letters, digits, `_`, `.`, `-`. |
| `artifact_patterns` | string list; **required, nonempty** | ZIP asset file names with `{version}` **exactly once** and no `/`, ending in `.zip`. Matched to release versions; exactly one matching asset per release is required. Multiple patterns support historical renames. |
| `checksum_assets` | string list; `["SHA256SUMS.txt"]` | Exact release-asset names sought for checksum verification. An asset's SHA-256 may also come from the GitHub API; releases without a usable digest are not installed. |
| `exclude` | string list; `[]` | Archive exclusion patterns (e.g. `"build/**"`). For patterns not starting with `*`, validation requires a relative path without parent traversal after stripping a trailing `/**`; `"*"`, `"**"`, `"**/*"` and patterns starting with `index.html` are rejected to protect the entry page. |
| `icon` | string; `""` | Optional path within a release for the launcher icon; if nonempty, must be relative without parent traversal. |
| `notes` | string; `""` | App description/implementation notes in the definition. |
| `category` | string; `""` | Short launcher category. |
| `tagline` | string; `""` | One-line launcher description. |
| `color` | string; `""` | Launcher brand color; if nonempty, `#` followed by exactly six ASCII hexadecimal digits. |

For a concrete built-in override or additional-app template, see [config.example.toml](../config.example.toml). That annotated example shows all global defaults and every built-in override key; additional apps supply the three required definition keys and may use the other definition keys above.

## Process environment

These are read directly by the binary (not from `config.toml`). For path overrides, unset or whitespace-only values select the listed default. Paths for `DATA_DIR`, `STATE_DIR`, `CACHE_DIR`, `WORK_DIR`, the config file's parent and an enabled `LOG_DIR` must be absolute and separate, except that logs may be inside state.

| Variable | Default | Meaning |
| --- | --- | --- |
| `CRAFT_CONFIG` | `/config/config.toml` | Operator config file. Its parent is `CONFIG_DIR`. |
| `CRAFT_DATA_DIR` | `/srv/data` | Published releases and staging. |
| `CRAFT_STATE_DIR` | `/srv/state` | Updater state, locks, history, heartbeat and user database. |
| `CRAFT_CACHE_DIR` | `/srv/cache` | Verified archives and GitHub API cache. |
| `CRAFT_WORK_DIR` | `/srv/work` | Temporary in-progress downloads. |
| `CRAFT_LOG_DIR` | unset | Optional file logging directory. Without it, logs go to stderr only. |
| `CRAFT_LOG_LEVEL` | `info` | `debug`, `warn`, `error` or `info`; case-insensitive, with other values (including `trace`) falling back to `info`. Only `craft_host` log targets are emitted. |
| `FILE_UMASK` | inherited process umask (Compose: `0022`) | Optional octal mask of up to four digits; invalid values or masks removing owner permissions are rejected. Applies to server-side file/directory creation. The sample `.env.example` sets `0002` instead. `doctor` also reads it. |
| `CRAFT_ALLOW_ROOT` | unset | Mutating commands refuse to run as root unless exactly `1`. `doctor` still reports root as a failure. Run as a non-root identity instead. |

**Doctor-only variables:** `RUN_UID` and `RUN_GID` are optional decimal numeric identities checked against the actual process UID/GID for diagnostic warnings (zero triggers a warning); they do **not** switch the binary's identity. Compose uses them in its `user:` setting and passes them to doctor. `CRAFT_HOST_CONFIG_DIR`, `CRAFT_HOST_DATA_DIR`, `CRAFT_HOST_STATE_DIR`, `CRAFT_HOST_CACHE_DIR`, `CRAFT_HOST_WORK_DIR`, and optionally `CRAFT_HOST_LOG_DIR` are host-side paths used for overlap checks and suggested preparation commands; they do not change container paths. Compose passes these from `.env` `CONFIG_DIR`, `DATA_DIR`, `STATE_DIR`, `CACHE_DIR`, `WORK_DIR`, and, with `compose.logs.yaml`, `LOG_DIR`. `IMAGE`, `PORT`, `BIND_ADDRESS`, `LOG_LEVEL`, `WORKING_DIR`, `SUPPLEMENTAL_GID` and `COMPOSE_FILE` in `.env` are Compose settings, not direct process variables; see [.env.example](../.env.example).

## Persistent files and permissions

- `STATE_DIR/apps/<id>.json`: per-app installed/active/pending release state, pins, blocked versions, check results and failures. `STATE_DIR/history.jsonl`: update events, trimmed after exceeding 2 MiB to the last 5000 lines. `STATE_DIR/locks/updater.lock`: advisory cross-process lock; `STATE_DIR/heartbeat.json`: updater status and timestamp.
- `STATE_DIR/users.sqlite3`: SQLite accounts, password hashes, roles and linked OIDC identities; SQLite WAL operation can create `users.sqlite3-wal` and `users.sqlite3-shm`. On creation the database is set to mode `0660`; doctor rejects access by users outside its owner/group. Keep state private and persistent.
- `DATA_DIR/<release_dir>/<version>/`: immutable installed static site and `.craft-release.json` release marker. `DATA_DIR/<release_dir>/current` is the atomically swapped symlink to the active version. `DATA_DIR/.staging/` holds unpublished validated sites and temporary trash; it must share a filesystem with release directories for atomic promotion. Published release contents are made read-only; retain data to survive restarts.
- `CACHE_DIR/archives/<id>/<asset name>`: downloaded ZIPs, verified before reuse, pruned by `cache_max_size`. `CACHE_DIR/api/<owner>__<repo>.json`: cached release list with ETag for conditional GitHub API requests; separate from the archive-size limit.
- `WORK_DIR/downloads/<asset name>.<random>.part`: in-progress downloads; interrupted temporary files are cleaned at startup. `WORK_DIR` and `CACHE_DIR` need not be on the data filesystem.
- If `CRAFT_LOG_DIR` is set, `LOG_DIR/updater.log` supplements stderr; on exceeding `log_max_size` it rotates to `updater.log.1`, through `updater.log.<log_backups>` (or discards old logs if backups is zero). Enable the optional Compose mount with `compose.logs.yaml` to persist these logs.

Ordinary files and directories are created with requested modes `0666` and `0777` reduced by the process umask, unless a particular file is further restricted (notably the SQLite database and read-only published releases). Mount `CONFIG_DIR` read-only, keep secrets there readable only by the server identity, and make the data/state/cache/work directories writable by that identity. `craft-host doctor` checks config, identity, directory access, free space and secrets without changing ownership or permissions.
