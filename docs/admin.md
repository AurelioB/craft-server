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

The interface is **disabled by default** (`/admin` answers 404). Enable it with a sign-in
method and a first administrator:

```toml
[auth]
methods = ["local"]      # how people sign in, see below

[admin]
enabled = true
```

```sh
docker compose exec -T host craft-host user add alice --role admin --email alice@example.net \
  <<<'a long passphrase'
```

Only accounts with the **admin** role may open it; others get 403. Besides the apps it manages
the [accounts](#accounts).

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

A page load without a session (local, OIDC) goes to the sign-in page and returns to the
requested page afterwards; other requests (scripts, images, `status.json`) get 401, Basic asks
the browser for credentials, and proxy mode answers 403 without an identity. `/healthz`,
`/readyz/…`, `/auth/…`, the launcher's logos and fonts, and apps' `*.webmanifest` files stay
reachable.
Browsers fetch manifests without cookies; they describe the app (EffectCraft 0.6.0: name, short
name, description, categories, icons, colors, start URL, scope), so anyone who can reach the
server can read those. When signed in, the launcher shows the account, its role, a link to
`/admin` for admins, **Link** *provider* for accounts without a linked OIDC identity, and a
sign-out button.

Signing in controls what the server sends, not what a browser already has: after sign-out or
session expiry, tabs that are already open keep running, and files the browser cached (release
files are cached as immutable) or that an app's service worker stored can still be used on that
device. New requests to the server need a fresh sign-in.

## Accounts

Accounts live in SQLite at `STATE_DIR/users.sqlite3` (mode 0660, readable by the service's user
and group only; back it up with `STATE_DIR`). Each has a user name, an optional e-mail address,
an optional password (Argon2id), a role and an optional linked OpenID Connect identity (issuer
and subject).

In `/admin` → **Users** administrators add accounts, change e-mail, role and password, remove a
linked identity, and delete accounts. On the command line (works while the server runs):

| Command | Effect |
| --- | --- |
| `craft-host user list` | Accounts with role, e-mail, sign-in ways and last sign-in |
| `craft-host user add NAME [--email E] [--role user\|admin] [--no-password]` | Add; password from standard input |
| `craft-host user set-password NAME` | New password from standard input |
| `craft-host user set-role NAME ROLE` / `set-email NAME [E]` | Change role / e-mail (none: remove) |
| `craft-host user unlink NAME` / `delete NAME` | Remove the linked identity / the account |
| `craft-host user import FILE` | Add `name:<argon2 hash>[:role]` lines, e.g. an earlier users file |

Run them with `docker compose exec -T host craft-host user …` (`-T` passes standard input).

- Passwords need at least 8 characters. Failed sign-ins are logged and delayed; after 10
  failures within 15 minutes an address is refused for a while.
- Changes apply at once to existing sessions, also when made from the CLI: a new role is used on
  the next request; a new password, a removed identity or a deleted account ends that account's
  sessions.
- The last administrator cannot be demoted or deleted in **Users** or with the CLI, and nobody
  can delete their own account. Exception, by design: with provider roles (`admin_groups` etc.)
  an OIDC sign-in applies the provider's role even to the last administrator.
- Recovery: `craft-host user set-password NAME` for a lost password, or
  `craft-host user add NAME --role admin` for a new administrator. With `methods = ["oidc"]`
  only, passwords do not help: promote an account that is linked to the provider with
  `craft-host user set-role NAME admin` (or fix the provider's groups when roles come from it).
- Sessions are kept in memory: a restart signs everyone out.

## Sign-in methods

Set `[auth] methods` in `config.toml`:

| `methods` | How it works | Roles from |
| --- | --- | --- |
| `[]` (default) | Nobody signs in; with `/admin` enabled every visitor is an administrator | — (only behind a proxy that authenticates `/admin` itself) |
| `["local"]` | Sign-in page with user name and password | accounts |
| `["oidc"]` | OpenID Connect; page loads go straight to the provider | accounts, or the provider's groups |
| `["local", "oidc"]` | Sign-in page offering both | as above |
| `["basic"]` | HTTP Basic against the accounts (browser prompt, scripts) | accounts |
| `["proxy"]` | User and groups from headers set by a trusted reverse proxy; not stored as accounts | the proxy's groups |

`apps = "signed-in"` needs a sign-in method.

### OpenID Connect

```toml
[auth]
methods = ["local", "oidc"]         # or ["oidc"]
apps = "signed-in"                  # optional

[auth.oidc]
name = "Authentik"                  # "Sign in with Authentik"
issuer = "https://auth.example.net/application/o/craft-apps"
client_id = "craft-apps"
client_secret_file = "oidc-client-secret"   # in CONFIG_DIR; omit for a public client
redirect_url = "https://apps.example.net/auth/oidc/callback"
```

Which account a provider identity signs in to:

1. the account linked to that identity (issuer + subject);
2. otherwise, only with `link_by_email = true`: the account with the same e-mail address, if the
   provider marks the address verified and the account has no linked identity yet. Off by
   default, because it trusts the provider to verify addresses and never to reassign them;
3. otherwise, with `create_users = true` (default), a new account named after
   `preferred_username` (made unique, e.g. `ana-2`) with the user role; with `false`, sign-in is
   refused.

To link an existing account explicitly, its owner signs in with the password and chooses
**Link** *provider* in the launcher (`/auth/oidc/login?link=1`); the identity is then tied to
that account, and administrators see it in **Users**. An identity belongs to one account.

Roles: by default they are managed in **Users** like any other account. To let the provider
decide instead, set any of `admin_users`, `admin_groups` or `user_groups`: at every OIDC
sign-in, an account named in `admin_users` or in a group of `admin_groups` becomes an admin;
otherwise a user if `user_groups` is empty or it is in one of those groups; otherwise sign-in
is refused. The provider can then also demote the last administrator; its groups can promote
someone again. In Authentik, for example, create the groups `craft-admins` and `craft-users`
and assign people to them; its default `profile` scope mapping sends them in the `groups` claim.

Register `redirect_url` with the provider; it must be the address people use, because the
session cookie is set for that host. A sign-in started on another host name for the same server
(for example a `*.lan` alias behind a local proxy) is sent to `redirect_url`'s host first and
continues there, so after signing in people use the canonical address. The server reads the provider metadata from
`<issuer>/.well-known/openid-configuration`, sends the browser to the authorization endpoint with
`state`, `nonce` and a PKCE S256 challenge (the `state` is also bound to the browser by a
cookie), and exchanges the code at the token endpoint (client secret via HTTP Basic). It checks
the ID token's issuer, audience, authorized party, expiry and nonce. The ID token comes straight
from the token endpoint over verified TLS, which OpenID Connect Core §3.1.3.7 accepts in place of
a signature check. The subject comes from `sub`, the user name from `preferred_username`
(`username_claim`), groups from `groups` (`groups_claim`), and `email`/`email_verified` from the
same-named claims.

### Trusted proxy (forward auth)

```toml
[server]
trusted_proxies = ["172.16.0.0/12"]   # addresses of your reverse proxy

[auth]
methods = ["proxy"]
admin_groups = ["craft-admins"]       # required while [admin] is enabled
user_groups = ["craft-users"]

[auth.proxy]
user_header = "X-authentik-username"
groups_header = "X-authentik-groups"  # '|' or ',' separated
```

Roles follow the group rules above, per request; proxy identities are not stored as accounts.
Identity headers are believed **only** from peers in `trusted_proxies`; a request from any other
address gets 403 regardless of its headers. Make sure the proxy overwrites these headers and that
the port is not reachable around it.

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
on every other host name `/admin` answers 404. Sign-in (`/auth/`) works on both with `local`,
`basic` and `proxy`, with a separate session per host name; `oidc` cannot be combined with
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

- Every admin action, including account changes, is a `POST` that must carry the `craft_csrf`
  cookie value in an `X-Craft-CSRF` header (double submit); a browser `Origin` from another host
  is refused. This applies in every method, including none, Basic and proxy, where browsers
  attach credentials automatically. Sign-in and sign-out refuse a foreign `Origin`.
- Password hashes never leave the server; the account list carries whether a password is set.
- The session cookie `craft_session` is `HttpOnly`, `SameSite=Lax` and scoped to `/`, so links to
  an app from another site and the return from the identity provider keep the session. The CSRF
  cookie is `SameSite=Strict` and scoped to `/admin`. `cookie_secure = "auto"` marks both
  `Secure` when the client used HTTPS according to `X-Forwarded-Proto` from a trusted proxy; use
  `always` behind HTTPS proxies that are not listed in `trusted_proxies`.
- After sign-in the server only redirects to a path on this site (`next` targets such as
  `//other.example` fall back to the launcher).
- Admin and sign-in pages are sent with `X-Frame-Options: DENY` and `Cache-Control: no-store`.
- With no sign-in method and `/admin` enabled, a warning is logged at startup and reported by
  `doctor`; `doctor` also fails when no administrator can sign in with the configured methods.

## Upgrading from earlier sign-in settings

Older configurations are rejected with a message naming each moved key:

- `[auth] method = "form"` becomes `methods = ["local"]`; `"oidc"`, `"basic"`, `"proxy"` become
  one-element lists; `"none"` becomes `[]`.
- `[auth] users_file` is gone; import the file once, then delete the setting:
  `docker compose exec -T host craft-host user import /config/users` (roles carry over).
- With OIDC, `admin_groups`/`admin_users` are now optional (without them roles are managed in
  **Users**).
- From the first version with `[admin] auth`: `[admin] auth` becomes `[auth] methods` plus
  `[admin] enabled = true`; `users_file`, `session_ttl`, `cookie_secure`, `[admin.oidc]` and
  `[admin.proxy]` move to `[auth]`; `allowed_users`/`allowed_groups` become
  `admin_users`/`admin_groups`; `listen` and `shared_origin` are gone; the OIDC redirect URL ends
  in `/auth/oidc/callback`.
