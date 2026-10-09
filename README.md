# Craft Apps Host

Run the seven official Craft browser apps on your own server: PhotoCraft, VectorCraft,
FilmCraft, LightCraft, PdfCraft, EffectCraft and DesignCraft. A launcher at `/` links to
each app. `craft-host` downloads official GitHub releases and checks for updates in the
background; app updates do not require a new container image. Optional sign-in provides
`user` and `admin` roles and an administration interface at `/admin/`.

The apps run in your browser. The host serves app files, **not documents**: it does not
provide server-side document storage, sharing or synchronization, and the apps cannot
browse folders on the server.

- One container serves the launcher and apps, manages releases and, when enabled, `/admin/`.
- Stable app URLs (for example `/photocraft/`) point to the active release; retained
  versions are available at `/photocraft/<version>/`.
- Background checks continue on a per-app schedule across restarts. Updates are
  checksum-verified and can activate immediately or after an idle period.
- `/admin/` can check, update, apply, pin, roll back and manage users. It is disabled
  by default; the launcher and apps are public by default.
- Local accounts, OpenID Connect, HTTP Basic and trusted reverse-proxy sign-in are
  supported. No Docker socket, privileged container, GPU or database server is needed.

## Quick start with Docker Compose

With Docker Compose v2, clone the [repository](https://github.com/AurelioB/craft-server):

```sh
git clone https://github.com/AurelioB/craft-server.git
cd craft-server
```

Alternatively, download [`compose.yaml`](compose.yaml), [`.env.example`](.env.example)
and [`config.example.toml`](config.example.toml) into one directory. From the
checkout or that directory:

```sh
cp .env.example .env
# Edit .env: choose a non-root RUN_UID/RUN_GID and host paths.
# With the example values (uid 1000, gid 10000, /srv/craft-apps):
sudo install -d -o 1000 -g 10000 -m 2775 \
  /srv/craft-apps/config /srv/craft-apps/data /srv/craft-apps/state \
  /srv/craft-apps/cache /srv/craft-apps/work
# The container user must be able to read config.toml (it holds no secrets by itself):
sudo install -m 0640 -o 1000 -g 10000 config.example.toml /srv/craft-apps/config/config.toml
docker compose pull && docker compose up -d
```

Change the directory command to match your `.env` if you changed the sample paths or
identity. Bind-mount sources must exist; Compose does not create or chown them.
`/config` is read-only in the container; data, state, cache and work are writable by
the configured UID/GID. The container starts `/usr/local/bin/craft-host serve` and
listens on port 8080. Open `http://<server>:8080/`; the launcher may show apps still
being installed on first start. `docker compose logs -f host` shows startup and updates.
To build the image locally instead (with a full source checkout), use
`docker compose build && docker compose up -d`.

To enable `/admin/`, set `methods = ["local"]` in the existing `[auth]` table
and `enabled = true` in the existing `[admin]` table of `config.toml`, then
restart and add an administrator; replace `NAME` and the sample password:

```sh
docker compose restart host
docker compose exec -T host craft-host user add NAME --role admin <<<'a long passphrase'
```

Use HTTPS before sending real passwords across a network. See
[sign-in and administration](docs/admin.md) for roles, OIDC and proxy setup.

### Without Compose

After preparing the five host directories and `config.toml` as above, the equivalent
container can be run directly (replace the sample UID/GID and paths to match yours):

```sh
docker run -d --name craft-host --init --restart unless-stopped \
  --user 1000:10000 --group-add 10000 \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  --workdir /srv/work --tmpfs /tmp:mode=1777,size=16m \
  -p 0.0.0.0:8080:8080 \
  -e FILE_UMASK=0002 -e CRAFT_LOG_LEVEL=info \
  -e RUN_UID=1000 -e RUN_GID=10000 \
  -e CRAFT_HOST_CONFIG_DIR=/srv/craft-apps/config \
  -e CRAFT_HOST_DATA_DIR=/srv/craft-apps/data \
  -e CRAFT_HOST_STATE_DIR=/srv/craft-apps/state \
  -e CRAFT_HOST_CACHE_DIR=/srv/craft-apps/cache \
  -e CRAFT_HOST_WORK_DIR=/srv/craft-apps/work \
  --mount type=bind,src=/srv/craft-apps/config,dst=/config,readonly \
  --mount type=bind,src=/srv/craft-apps/data,dst=/srv/data \
  --mount type=bind,src=/srv/craft-apps/state,dst=/srv/state \
  --mount type=bind,src=/srv/craft-apps/cache,dst=/srv/cache \
  --mount type=bind,src=/srv/craft-apps/work,dst=/srv/work \
  ghcr.io/aureliob/craft-host:latest
```

The image defaults to `serve` and port 8080. To keep rotated log files as well as
container logs, mount a writable log directory at `/srv/logs` and set
`CRAFT_LOG_DIR=/srv/logs` (Compose uses `compose.logs.yaml` for this).

### Without Docker

Download a static Linux musl archive for your architecture from
[Releases](https://github.com/AurelioB/craft-server/releases):
`craft-host-X.Y.Z-x86_64-linux-musl.tar.gz` or
`craft-host-X.Y.Z-aarch64-linux-musl.tar.gz`. Check it against the release's
`SHA256SUMS`, extract `craft-host`, and install it in your `PATH`. Alternatively,
build from the repository:

```sh
cd host
cargo build --release --locked
# executable: target/release/craft-host
```

Run as a non-root user who can read the config and write the other directories.
Without overrides, the binary expects `/config/config.toml`, `/srv/data`,
`/srv/state`, `/srv/cache` and `/srv/work`. Set the paths for your machine:

```sh
export CRAFT_CONFIG=/etc/craft-host/config.toml
export CRAFT_DATA_DIR=/var/lib/craft-host/data
export CRAFT_STATE_DIR=/var/lib/craft-host/state
export CRAFT_CACHE_DIR=/var/cache/craft-host
export CRAFT_WORK_DIR=/var/lib/craft-host/work
export FILE_UMASK=0002
craft-host serve
```

Create the directories and copy the repository-root `config.example.toml` to
`CRAFT_CONFIG` first. For a persistent service, a short systemd unit example
(after creating the `craft-host` user and writable paths):

```ini
[Unit]
Description=Craft Apps Host

[Service]
User=craft-host
Group=craft-host
EnvironmentFile=/etc/craft-host/host.env
ExecStart=/usr/local/bin/craft-host serve
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

The environment file contains the assignments above without `export`.
See [installation](docs/install.md) for permissions and HTTPS.

## Configuration

Compose reads `.env` for *host-side* paths and container settings. Sample values
below are from `.env.example`; defaults shown in parentheses come from Compose.

| Compose / `.env` variable | Meaning |
| --- | --- |
| `RUN_UID`, `RUN_GID` | Required non-root numeric user/group in the container (sample `1000:10000`); own the writable host directories. |
| `SUPPLEMENTAL_GID` | Optional extra group; defaults to `RUN_GID`. |
| `FILE_UMASK` | Permissions mask for created files; sample `0002` (Compose fallback `0022`). |
| `CONFIG_DIR` | Required host config directory, mounted read-only at `/config`; sample `/srv/craft-apps/config`. |
| `DATA_DIR` | Required host releases directory, mounted at `/srv/data`; sample `/srv/craft-apps/data`. |
| `STATE_DIR` | Required host state directory, mounted at `/srv/state`; sample `/srv/craft-apps/state`. |
| `CACHE_DIR` | Required host archive/API cache, mounted at `/srv/cache`; sample `/srv/craft-apps/cache`. |
| `WORK_DIR` | Required host download work directory, mounted at `/srv/work`; sample `/srv/craft-apps/work`. |
| `LOG_DIR` | Optional host log directory mounted at `/srv/logs` with the logs overlay. |
| `COMPOSE_FILE` | Set to `compose.yaml:compose.logs.yaml` to enable persistent logs; otherwise just `compose.yaml`. |
| `BIND_ADDRESS`, `PORT` | Published host-side HTTP address and port; default `0.0.0.0:8080`. |
| `IMAGE` | Image to pull/run; default `ghcr.io/aureliob/craft-host:latest`, e.g. pin `ghcr.io/aureliob/craft-host:0.1.0`. |
| `LOG_LEVEL` | Process log level passed as `CRAFT_LOG_LEVEL`; default `info`. |
| `WORKING_DIR` | Container process working directory; default `/srv/work` (not the `WORK_DIR` host path). |

The process itself reads these variables; they also work without Compose:

| Process variable | Meaning / default |
| --- | --- |
| `CRAFT_CONFIG` | Config file path; `/config/config.toml`. |
| `CRAFT_DATA_DIR`, `CRAFT_STATE_DIR` | Releases and state; `/srv/data`, `/srv/state`. |
| `CRAFT_CACHE_DIR`, `CRAFT_WORK_DIR` | Cache and work; `/srv/cache`, `/srv/work`. |
| `CRAFT_LOG_DIR` | Optional directory for rotated `updater.log` in addition to stderr; unset by default. |
| `CRAFT_LOG_LEVEL` | `debug`, `info` (default), `warn` or `error`. |
| `FILE_UMASK` | Octal creation mask; if unset, use the process umask. |
| `CRAFT_ALLOW_ROOT` | `1` permits mutating commands as root; normally run as a non-root user. |

Compose also passes `RUN_UID`, `RUN_GID` and `CRAFT_HOST_CONFIG_DIR`,
`CRAFT_HOST_DATA_DIR`, `CRAFT_HOST_STATE_DIR`, `CRAFT_HOST_CACHE_DIR` and
`CRAFT_HOST_WORK_DIR` to `doctor` for host-side diagnostics; the optional logs
overlay passes `CRAFT_HOST_LOG_DIR`. These do not change the process data paths.

`config.toml` is read at `CRAFT_CONFIG` (`CONFIG_DIR/config.toml` in Compose).
Every setting is optional and unknown keys are rejected. Put a GitHub API token
or OIDC client secret in separate files in `CONFIG_DIR` and reference them from
`[updater] github_token_file` or `[auth.oidc] client_secret_file`. The account
database is `STATE_DIR/users.sqlite3` and should be backed up with the state.
A small example (edit the existing sections in `config.example.toml`, rather
than appending duplicate tables):

```toml
[auth]
methods = ["local"]             # default: [] (no sign-in)
apps = "public"                 # "signed-in" requires a sign-in method

[admin]
enabled = true                  # default: false

[retention]
keep_latest = 2                 # default: 1, counting the active release
keep_days = 0                   # default: 0
keep_recently_used = "0"        # default: no grace period

[updater]
activation = "immediate"        # or "idle" (wait for no app requests)
idle_after = "30m"              # used with "idle"
```

Checks run per app every hour by default and continue their schedule after
restarts. See the complete [configuration reference](docs/configuration.md)
for every key, built-in app setting, default and configuration file.

## Updating the host

App releases update in the background; upgrading `craft-host` itself is separate.
With Compose, pull a newer published image and restart:

```sh
docker compose pull && docker compose up -d
```

Set `IMAGE=ghcr.io/aureliob/craft-host:0.1.0` in `.env` to pin a release;
change it when you choose to upgrade. A `vX.Y.Z` release publishes `X.Y.Z`,
`X.Y` and `latest` image tags for `linux/amd64` and `linux/arm64`. Pushes to
`main` publish `edge` rather than `latest`. For a local build, pull source
changes and run `docker compose build && docker compose up -d`; for a
standalone install, replace the binary and restart your service. Keep backups
of the state and configuration directories; see [operations](docs/operations.md).

## Command line

Run `docker compose exec host craft-host <command>` or invoke `craft-host`
directly. `craft-host --help` lists commands and `craft-host --version` prints
the installed version.

| Command | Purpose |
| --- | --- |
| `serve` | Serve apps and check for updates on schedule. |
| `status [--json]` | Show installed, active, latest and pending versions, pins and failures. |
| `doctor [--offline]` | Check configuration, permissions, identities, space and connectivity. |
| `check [APP]` / `update [APP]` | Look for or install the newest eligible release. |
| `apply APP` | Activate a release pending idle activation. |
| `pin APP VERSION` / `unpin APP` | Hold an app on a release / follow new releases again. |
| `rollback APP [VERSION]` / `allow APP VERSION` | Restore a retained release / permit a blocked one. |
| `history [APP]` | Recent update history. |
| `user list\|add\|set-password\|set-role\|set-email\|unlink\|delete\|import` | Manage accounts; passwords are read from standard input. |
| `healthcheck` | Check `/healthz` (used by the container health check). |

## HTTPS and documentation

Plain HTTP over a LAN is not a secure browser context: some app capabilities,
including WebGPU and browser file storage, may be unavailable, and passwords
are unprotected in transit. Use an HTTPS reverse proxy with a certificate your
devices trust, or a protected tunnel. See [installation](docs/install.md) for
proxy and browser details.

- [Configuration reference](docs/configuration.md)
- [Installation, directories and HTTPS](docs/install.md)
- [Sign-in, roles and administration](docs/admin.md)
- [Users, groups and permissions](docs/permissions.md)
- [Operations: updates, retention, backup and recovery](docs/operations.md)
- [Apps and browser storage](docs/apps.md)
- [Architecture](docs/architecture.md)
- [Verification record](docs/verification.md)
- [Original project plan](docs/plan.md)

## Development

From `host/` (requires Rust and, for the ignored integration test, Docker Compose):

```sh
cargo test
cargo test --test compose -- --ignored --nocapture
```
