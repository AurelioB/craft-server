# Sign-in, roles and the administration interface

Everything is served on one port: the launcher, the apps, sign-in under `/auth/` and the
administration interface under `/admin/`.

## Administration interface

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

The interface is **disabled by default** (`/admin` answers 404). Enable it with:

```toml
[auth]
method = "form"          # how people sign in, see below

[admin]
enabled = true
```

Only accounts with the **admin** role may open it; others get 403.

> **Trust note.** On the apps' host name, `/admin` shares the apps' browser origin. If a
> downloaded app release were malicious, its script could use a signed-in administrator's
> session to run update actions. Admins who want to rule that out give `/admin` its own host name
> on the same port, see [Same origin as the apps](#same-origin-as-the-apps).

## Roles and protected apps

| Role | Launcher and apps | `/admin` |
| --- | --- | --- |
| `user` | yes | no |
| `admin` | yes | yes |

The launcher and the apps are public by default. To require a signed-in account with either
role:

```toml
[auth]
apps = "signed-in"
```

A page load without a session (form, OIDC) goes to the sign-in page and returns to the requested
page afterwards; other requests (scripts, images, `status.json`) get 401, Basic asks the browser
for credentials, and proxy mode answers 403 without an identity. `/healthz`, `/readyz/…`,
`/auth/…`, the launcher's logos and fonts, and apps' `*.webmanifest` files stay reachable.
Browsers fetch manifests without cookies; they describe the app (EffectCraft 0.6.0: name, short
name, description, categories, icons, colors, start URL, scope), so anyone who can reach the
server can read those. When signed in, the launcher shows the account, its role, a link to
`/admin` for admins and a sign-out button.

Signing in controls what the server sends, not what a browser already has: after sign-out or
session expiry, tabs that are already open keep running, and files the browser cached (release
files are cached as immutable) or that an app's service worker stored can still be used on that
device. New requests to the server need a fresh sign-in.

## Sign-in methods

Set `[auth] method` in `config.toml`:

| Method | How it works | Roles from |
| --- | --- | --- |
| `none` (default) | Nobody signs in; with `/admin` enabled every visitor is an administrator | — (only behind a proxy that authenticates `/admin` itself) |
| `basic` | HTTP Basic against `users_file` (browser prompt) | users file |
| `form` | Login page at `/auth/login`, session cookie, against `users_file` | users file |
| `oidc` | OpenID Connect authorization-code flow with PKCE | groups claim |
| `proxy` | User and groups from headers set by a trusted reverse proxy | groups header |

`apps = "signed-in"` needs a method other than `none`.

### Users file (`basic`, `form`)

```sh
docker compose exec -T host craft-host hash-password <<<'a long passphrase'
# append "name:<hash>:admin" or "name:<hash>:user" to CONFIG_DIR/users ([auth] users_file)
chmod 0640 /srv/craft-apps/config/users
```

The role is the third field; a line without one gets `user`, a line with an unknown role is
ignored and reported by `doctor`. Hashes are Argon2id. The file is re-read when it changes;
removing a line revokes Basic access at once and blocks new form logins (existing sessions end at
`session_ttl`, default 12 h, at sign-out, or at restart). Failed logins are logged and delayed.

### OpenID Connect

```toml
[auth]
method = "oidc"
apps = "signed-in"                  # optional
admin_groups = ["craft-admins"]     # required while [admin] is enabled (or admin_users)
user_groups = ["craft-users"]       # empty = every account the provider signs in

[auth.oidc]
issuer = "https://auth.example.net/application/o/craft-apps"
client_id = "craft-apps"
client_secret_file = "oidc-client-secret"   # in CONFIG_DIR; omit for a public client
redirect_url = "https://apps.example.net/auth/oidc/callback"
```

Roles: an account named in `admin_users` or in a group of `admin_groups` is an admin; otherwise
it is a user if `user_groups` is empty or it is in one of those groups; otherwise sign-in is
refused. In Authentik, for example, create the groups `craft-admins` and `craft-users` and assign
people to them; its default `profile` scope mapping sends them in the `groups` claim.

Register `redirect_url` with the provider; it must be the address people use, because the
session cookie is set for that host. The server reads the provider metadata from
`<issuer>/.well-known/openid-configuration`, sends the browser to the authorization endpoint with
`state`, `nonce` and a PKCE S256 challenge, and exchanges the code at the token endpoint (client
secret via HTTP Basic). It checks the ID token's issuer, audience, authorized party, expiry and
nonce. The ID token comes straight from the token endpoint over verified TLS, which OpenID
Connect Core §3.1.3.7 accepts in place of a signature check. Groups come from the `groups` claim
(`groups_claim`), the user name from `preferred_username` (`username_claim`, falling back to
`sub`). Roles are fixed at sign-in; group changes apply at the next sign-in.

### Trusted proxy (forward auth)

```toml
[server]
trusted_proxies = ["172.16.0.0/12"]   # addresses of your reverse proxy

[auth]
method = "proxy"
admin_groups = ["craft-admins"]
user_groups = ["craft-users"]

[auth.proxy]
user_header = "X-authentik-username"
groups_header = "X-authentik-groups"  # '|' or ',' separated
```

Roles follow the same rules as OIDC, per request. Identity headers are believed **only** from
peers in `trusted_proxies`; a request from any other address gets 403 regardless of its headers.
Make sure the proxy overwrites these headers and that the port is not reachable around it.

## Same origin as the apps

`/admin` normally shares the apps' browser origin (same host name and port). App code is
downloaded from upstream; if a release were compromised, its script runs on that origin and could
use a signed-in administrator's session to call the admin API (CSRF tokens and cookie paths do
not stop same-origin scripts). Users without the admin role are not affected: the server refuses
admin requests from their sessions.

To keep administration out of the apps' origin while staying on the same port, give it a host
name of its own that resolves to this server, for example a LAN DNS entry:

```toml
[admin]
enabled = true
host = "admin.apps.example.net"
```

`/admin` then answers only on that host and the launcher, apps and status are not served there;
on every other host name `/admin` answers 404. Sign-in (`/auth/`) works on both with `basic`,
`form` and `proxy`, with a separate session per host name; `oidc` cannot be combined with
`[admin] host`, because sign-in completes on the single `redirect_url` host. The name must
resolve from the clients: `admin.localhost`, for example,
only works in a browser on the server itself (browsers resolve `*.localhost` to their own
loopback address). Behind a proxy listed in `[server] trusted_proxies`, `X-Forwarded-Host`
decides; from other peers it is ignored. Sibling host names are still the same *site*:
`apps.example.net` may set cookies for `example.net`, which could sign an administrator out but
not read the `HttpOnly` session or send an admin action.

None of this encrypts traffic. Over plain HTTP, passwords and session cookies cross the network
readable by anyone on the path; before using real credentials, put the server behind HTTPS
(reverse proxy) or reach it only through a protected tunnel or VPN.

## Request protections

- Every admin action is a `POST` that must carry the `craft_csrf` cookie value in an
  `X-Craft-CSRF` header (double submit); a browser `Origin` from another host is refused. This
  applies in every method, including `none`, Basic and proxy, where browsers attach credentials
  automatically. Sign-in and sign-out refuse a foreign `Origin`.
- The session cookie `craft_session` is `HttpOnly`, `SameSite=Lax` and scoped to `/`, so links to
  an app from another site and the return from the identity provider keep the session. The CSRF
  cookie is `SameSite=Strict` and scoped to `/admin`. `cookie_secure = "auto"` marks both
  `Secure` when the client used HTTPS according to `X-Forwarded-Proto` from a trusted proxy; use
  `always` behind HTTPS proxies that are not listed in `trusted_proxies`.
- After sign-in the server only redirects to a path on this site (`next` targets such as
  `//other.example` fall back to the launcher).
- Admin and sign-in pages are sent with `X-Frame-Options: DENY` and `Cache-Control: no-store`.
- With `method = "none"` and `/admin` enabled, a warning is logged at startup and reported by
  `doctor`.

## Upgrading from `[admin] auth`

Older configurations kept sign-in settings in `[admin]`; they are rejected with a message naming
each moved key. `[admin] auth` becomes `[auth] method` plus `[admin] enabled = true`;
`users_file`, `session_ttl`, `cookie_secure`, `[admin.oidc]` and `[admin.proxy]` move to `[auth]`;
`allowed_users`/`allowed_groups` become `admin_users`/`admin_groups`; `listen` and
`shared_origin` are gone. Add `:admin` to the users-file lines of administrators, and register the
new OIDC redirect URL ending in `/auth/oidc/callback`.
