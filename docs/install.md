# Installation and deployment

## Requirements

- Docker Engine with Compose v2 (Linux host recommended; see [permissions](permissions.md) for
  Docker Desktop and user-namespace notes).
- Outbound HTTPS to `api.github.com`, `github.com` and GitHub's asset host
  (`release-assets.githubusercontent.com` as of October 2026) for updates. Installed apps keep
  being served without it.
- An HTTPS reverse proxy for anything other than `localhost` (see [HTTPS](#https-and-the-reverse-proxy)).
- Disk: one release of each of the seven apps took about 280 MB in `DATA_DIR` and their archives
  about 160 MB in `CACHE_DIR` (October 2026 releases). Retention keeps several releases per app;
  the archive cache is bounded by `cache_max_size`.

## 1. Choose identities and directories

Edit `.env` (copy `.env.example`). All host paths are bind-mount sources; container-side paths
are fixed:

| Setting | Container path | Access | Holds |
| --- | --- | --- | --- |
| `CONFIG_DIR` | `/config` | updater, read-only | `config.toml`, optional GitHub token file |
| `DATA_DIR` | `/srv/data` | updater read-write, web read-only | releases, `public/` (status, entry links), `.staging/` |
| `STATE_DIR` | `/srv/state` | updater | per-app state, history, lock, heartbeat |
| `CACHE_DIR` | `/srv/cache` | updater | verified release archives, API response cache |
| `WORK_DIR` | `/srv/work` | updater | in-progress downloads |
| `LOG_DIR` (optional) | `/srv/logs` | updater | rotated `updater.log` |
| `WEB_WORKING_DIR` | — | — | working directory of nginx (default `/tmp`) |
| `UPDATER_WORKING_DIR` | — | — | working directory of the updater (default `/srv/work`) |

- Relative host paths resolve from the project directory; production setups should use absolute
  paths. Paths may contain spaces (no quoting or shell concatenation is involved).
- The directories must be separate and must not be nested in each other; `doctor` checks this.
- `WORK_DIR` and `CACHE_DIR` may be on other filesystems. Staging always happens inside
  `DATA_DIR/.staging` so that publishing is a single atomic rename on one filesystem.
- No document workspace is mounted: the browser apps cannot use server folders.

## 2. Prepare the directories

Compose is configured with `create_host_path: false`, so missing directories make
`docker compose up` fail instead of Docker creating root-owned ones. Nothing ever changes
ownership recursively. Create them once, owned by the updater identity:

```sh
sudo install -d -o "$RUN_UID" -g "$RUN_GID" -m 2775 \
  /srv/craft-apps/config /srv/craft-apps/data /srv/craft-apps/state \
  /srv/craft-apps/cache /srv/craft-apps/work
cp config.example.toml /srv/craft-apps/config/config.toml
```

Mode `2775` keeps the group shared and makes new files inherit it (setgid); use `2755` or `0750`
if the web server runs as the same user. See [permissions](permissions.md) for split identities.

Optional GitHub token (raises the API limit from 60 to 5000 requests/hour; not needed for
hourly checks of seven apps thanks to conditional requests):

```sh
install -m 0400 -o "$RUN_UID" /dev/stdin /srv/craft-apps/config/github-token <<<"github_pat_…"
# config.toml: [updater] github_token_file = "github-token"
```

The token is read by the updater only, never logged, and never mounted into the web container.

## 3. Build, check and start

```sh
docker compose build updater
docker compose run --rm --no-deps updater doctor --offline
docker compose up -d
docker compose exec updater craft-updater doctor     # includes GitHub and validation-listener checks
```

`doctor` exits non-zero on any `FAIL` and prints the preparation command for each problem.

## HTTPS and the reverse proxy

The web service publishes plain HTTP on `WEB_BIND_ADDRESS:WEB_PORT` (default
`127.0.0.1:8080`). Outside `localhost`, serve it through an HTTPS reverse proxy of your choice:
browsers only allow WebGPU, the clipboard, module workers in some apps and persistent storage in
a secure context.

- Redirects are relative, so the site works under any host name and behind a path prefix.
- Do not publish port 8081 (the private validation listener); Compose does not.
- Authentication, if wanted, belongs to the proxy. The updater has no network-facing interface.
- Browser storage is per origin: a LAN name and a public name for the same server have separate
  app libraries. Pick one canonical host name per app.
- `/healthz` (web liveness) and `/readyz/<app>` (app installed) are suitable for proxy health
  checks; neither reveals details.

## Upgrading this project

App updates are automatic. Updating the host itself (nginx image, updater code) is a separate
maintenance step:

```sh
git pull
docker compose build updater
docker compose up -d
```

Installed releases, state and cache are unaffected. Pin image digests in `.env`
(`WEB_IMAGE=nginxinc/nginx-unprivileged@sha256:…`) if you want fully reproducible runtimes.

## Persistent logs

Container logs (`docker compose logs updater`) are the default. To also keep rotated files:

```dotenv
LOG_DIR=/srv/craft-apps/logs
COMPOSE_FILE=compose.yaml:compose.logs.yaml
```

Create `LOG_DIR` like the other directories. Size and count follow `log_max_size` and
`log_backups` in `config.toml`.
