# Administration interface

`/admin/` shows every enabled app with its active, latest and pending release, pin, blocked
releases, activation policy, last check and last error, and offers:

| Action | Effect |
| --- | --- |
| Check | Look for a newer release without installing it |
| Update now | Install the newest eligible (or pinned) release and activate it immediately |
| Apply | Activate a release that is waiting for its app to be idle |
| Pin / Unpin | Hold the app on one release (installed if needed) / follow releases again |
| Roll back | Activate a retained older release; the current one is blocked from reinstalling |
| Allow | Permit a blocked release again |

Actions run one at a time in the background, under the same lock as scheduled updates and the
CLI; the page shows the running action, recent results and the update history. Every action is
recorded with the signed-in user.

The interface is **disabled by default** (`[admin] auth = "disabled"`; `/admin` answers 404).

## Authentication modes

Set `[admin] auth` in `config.toml`:

| Mode | How it works | Use when |
| --- | --- | --- |
| `none` | No authentication; every visitor of `/admin` is an administrator | Only behind a proxy that authenticates `/admin` itself |
| `basic` | HTTP Basic against `users_file` (browser prompt) | Simple setups, scripted access |
| `form` | Login page, session cookie, against `users_file` | Simple setups with a sign-out button |
| `oidc` | OpenID Connect authorization-code flow with PKCE | An identity provider such as Authentik, Keycloak, Authelia |
| `proxy` | User and groups from headers set by a trusted reverse proxy | Forward-auth setups (Authentik outpost, oauth2-proxy, Authelia) |

### Users file (`basic`, `form`)

```sh
docker compose exec -T host craft-host hash-password <<<'a long passphrase'
# append "name:<hash>" to CONFIG_DIR/admin-users (or the path in [admin] users_file)
chmod 0640 /srv/craft-apps/config/admin-users
```

Hashes are Argon2id. The file is re-read when it changes; removing a line revokes Basic access at
once and blocks new form logins (existing sessions end at `session_ttl`, default 12 h, or at
restart). Failed logins are logged and delayed.

### OpenID Connect

```toml
[admin]
auth = "oidc"
allowed_groups = ["craft-admins"]       # or allowed_users; both empty = any authenticated user

[admin.oidc]
issuer = "https://auth.example.net/application/o/craft-apps"
client_id = "craft-apps"
client_secret_file = "oidc-client-secret"   # in CONFIG_DIR; omit for a public client
redirect_url = "https://admin.apps.example.net/admin/oidc/callback"
```

Register `redirect_url` with the provider. The server reads the provider metadata from
`<issuer>/.well-known/openid-configuration`, sends the browser to the authorization endpoint with
`state`, `nonce` and a PKCE S256 challenge, and exchanges the code at the token endpoint
(client secret via HTTP Basic). It checks the ID token's issuer, audience, authorized party,
expiry and nonce. The ID token comes straight from the token endpoint over verified TLS, which
OpenID Connect Core §3.1.3.7 accepts in place of a signature check. Group membership comes from
the `groups` claim (`groups_claim`), the user name from `preferred_username` (`username_claim`,
falling back to `sub`).

### Trusted proxy (forward auth)

```toml
[server]
trusted_proxies = ["172.16.0.0/12"]   # addresses of your reverse proxy

[admin]
auth = "proxy"
allowed_groups = ["craft-admins"]

[admin.proxy]
user_header = "X-authentik-username"
groups_header = "X-authentik-groups"  # '|' or ',' separated
```

Identity headers are believed **only** from peers in `trusted_proxies`; a request from any other
address gets 403 regardless of its headers. Make sure the proxy overwrites these headers and that
the port is not reachable around it.

## Browser-origin isolation

App code is downloaded from upstream; if it were compromised, script running on the same origin
as `/admin` could use an administrator's signed-in session (path-scoped cookies and CSRF tokens do
not stop same-origin scripts). The interface therefore needs its own host name whenever it is
enabled:

```toml
[admin]
host = "admin.apps.example.net"
```

Route that name to the same server. `/admin` then answers only on that host, and nothing else
(launcher, apps, status) is served there; on every other host name `/admin` answers 404. Behind a
proxy listed in `[server] trusted_proxies`, `X-Forwarded-Host` decides; from other peers it is
ignored.

Configuration is rejected when `auth` is enabled without `host`, unless you opt in explicitly with
`shared_origin = true` (for example for a LAN-only setup). `doctor` warns about that opt-in.

## Request protections

- Every action is a `POST` that must carry the `craft_csrf` cookie value in an `X-Craft-CSRF`
  header (double submit); a browser `Origin` from another host is refused. This applies in every
  mode, including `none`, Basic and proxy, where browsers attach credentials automatically.
- Session and CSRF cookies are `SameSite=Strict` and scoped to `/admin`; the session cookie is
  `HttpOnly`. `cookie_secure = "auto"` marks them `Secure` when the client used HTTPS according
  to `X-Forwarded-Proto` from a trusted proxy; use `always` behind HTTPS proxies that are not
  listed in `trusted_proxies`.
- Admin pages are sent with `X-Frame-Options: DENY` and `Cache-Control: no-store`.
- With `auth = "none"`, a warning is logged at startup and reported by `doctor`.
