# Installation and deployment

## Requirements

- Docker Engine with Compose v2 (Linux host recommended; see [permissions](permissions.md) for
  Docker Desktop and user-namespace notes).
- Outbound HTTPS to `api.github.com`, `github.com` and GitHub's asset host
  (`release-assets.githubusercontent.com` as of October 2026) for updates. Installed apps keep
  being served without it.
- An HTTPS reverse proxy for anything other than `localhost` (see [HTTPS](#https-and-the-reverse-proxy)).
- Disk: one release of each of the seven apps took about 280 MB in `DATA_DIR` before
  precompression; the `.br`/`.gz` copies written at install time add roughly half of that. The
  downloaded archives took about 160 MB in `CACHE_DIR` (October 2026 releases). Retention keeps
  several releases per app; the archive cache is bounded by `cache_max_size`.

## 1. Choose identity and directories

Edit `.env` (copy `.env.example`). Host paths are bind-mount sources; container paths are fixed:

| Setting | Container path | Access | Holds |
| --- | --- | --- | --- |
| `CONFIG_DIR` | `/config` | read-only | `config.toml`, GitHub token, OIDC client secret |
| `DATA_DIR` | `/srv/data` | read-write | releases (`releases/<app>/<version>`, `current` pointers), `.staging/` |
| `STATE_DIR` | `/srv/state` | read-write | per-app state, history, lock, heartbeat, accounts (`users.sqlite3`) |
| `CACHE_DIR` | `/srv/cache` | read-write | verified release archives, API response cache |
| `WORK_DIR` | `/srv/work` | read-write | in-progress downloads |
| `LOG_DIR` (optional) | `/srv/logs` | read-write | rotated `updater.log` |
| `WORKING_DIR` | — | — | working directory of the process (default `/srv/work`) |

- Relative host paths resolve from the project directory; production setups should use absolute
  paths. Paths may contain spaces.
- The directories must be separate and not nested in each other; `doctor` checks this.
- `WORK_DIR` and `CACHE_DIR` may be on other filesystems. Staging always happens inside
  `DATA_DIR/.staging` so that publishing is one atomic rename on one filesystem.
- No document workspace is mounted: the browser apps cannot use server folders.

## 2. Prepare the directories

Compose uses `create_host_path: false`, so missing directories make `docker compose up` fail
instead of Docker creating root-owned ones. Nothing ever changes ownership. Create them once:

```sh
sudo install -d -o "$RUN_UID" -g "$RUN_GID" -m 2775 \
  /srv/craft-apps/config /srv/craft-apps/data /srv/craft-apps/state \
  /srv/craft-apps/cache /srv/craft-apps/work
cp config.example.toml /srv/craft-apps/config/config.toml
```

Optional GitHub token (raises the API limit from 60 to 5000 requests per hour, and unchanged
release lists then cost no quota). Hourly checks of seven apps use 7 requests per hour without
one, but the 60 are shared by everything behind the same public IP address:

```sh
install -m 0400 -o "$RUN_UID" /dev/stdin /srv/craft-apps/config/github-token <<<"github_pat_…"
# config.toml: [updater] github_token_file = "github-token"
```

Secrets in `CONFIG_DIR` are never logged, printed or served.

## 3. Build, check and start

```sh
docker compose build
docker compose run --rm host doctor --offline
docker compose up -d
docker compose exec host craft-host doctor     # includes GitHub and OIDC connectivity
```

`doctor` exits non-zero on any `FAIL` and prints a preparation command for each problem. To
enable the administration page or require sign-in for the apps, follow [admin.md](admin.md).

## HTTPS and the reverse proxy

The container publishes plain HTTP on `BIND_ADDRESS:PORT` (default `0.0.0.0:8080`, every host
interface). Docker-published ports bypass host firewalls such as ufw; set `BIND_ADDRESS` to one
address (for example the LAN address, or `127.0.0.1` behind a local proxy) to narrow it.

Plain HTTP from another machine is not a secure context. Observed in Chromium over
`http://<LAN address>`: all seven apps load and show their start screens, but `navigator.gpu`
(WebGPU) and the Origin Private File System are unavailable, as is the clipboard API. According
to the apps' own hosting notes, rendering then falls back to WebGL2, and EffectCraft and LightCraft
fall back from OPFS to IndexedDB (EffectCraft: or memory only, which keeps nothing after the tab
closes). Saving and reopening projects over plain HTTP has not been tested. For full
functionality on the LAN, serve it through an HTTPS reverse proxy with a certificate the clients
trust:

- Redirects are relative, so the site works under any host name and behind a path prefix.
- List the proxy in `[server] trusted_proxies` so `X-Forwarded-Proto/-Host` are honoured (secure
  cookies, admin host matching) and, with `[auth] methods = ["proxy"]`, its identity headers.
- `/admin` is on the apps' port; optionally give it a host name of its own (`[admin] host`), see
  [admin.md](admin.md#same-origin-as-the-apps).
- Browser storage is per origin: a LAN name and a public name for the same server have separate
  app libraries. Pick one canonical host name per app.
- `/healthz` (process alive) and `/readyz/<app>` (app installed) suit proxy health checks.

## Upgrading this project

App updates are automatic. Updating the host itself is a separate maintenance step:

```sh
git pull
docker compose build
docker compose up -d
```

Installed releases, state and cache are kept. Releases installed by earlier versions get their
precompressed copies added in the background after the upgrade.

Configurations with older sign-in settings (`[auth] method`, `users_file`, or `[admin] auth`, …)
are rejected with a message per moved key; see
[admin.md](admin.md#upgrading-from-earlier-sign-in-settings). A users file is imported once with
`craft-host user import`.

### From the two-service layout (web + updater)

Earlier versions ran nginx and the updater as separate services. To upgrade:

1. In `.env`: replace `WEB_BIND_ADDRESS`/`WEB_PORT` with `BIND_ADDRESS`/`PORT`; remove
   `WEB_UID`, `WEB_GID`, `UPDATER_UID`, `UPDATER_GID` and the `*_SUPPLEMENTAL_GID` settings
   (use `SUPPLEMENTAL_GID` if needed); replace `UPDATER_WORKING_DIR` with `WORKING_DIR`.
2. In `config.toml`: delete `[updater] validation_url` (unknown keys are rejected).
3. `docker compose down --remove-orphans && docker compose build && docker compose up -d`.
4. Optionally delete `DATA_DIR/public`; it is no longer used.

Releases, pins, blocks and history carry over.

## Persistent logs

Container logs (`docker compose logs host`) are the default. To also keep rotated files:

```dotenv
LOG_DIR=/srv/craft-apps/logs
COMPOSE_FILE=compose.yaml:compose.logs.yaml
```

Create `LOG_DIR` like the other directories. Size and count follow `log_max_size` and
`log_backups` in `config.toml`.
