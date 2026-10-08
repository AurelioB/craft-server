# Verification record

Recorded 8 October 2026 on Linux (Docker 29.8, Compose 5.6, Rust 1.99), for the single-binary
`craft-host` image, against the official releases listed in [apps.md](apps.md). Packaging and
serving checks do not guarantee that every upstream editing function works.

## Automated tests

`cd host && cargo test` runs the unit, lifecycle and HTTP/admin suites;
`cargo test --test compose -- --ignored` runs the Docker Compose integration test. All passed.

| Suite | Count | Covers |
| --- | --- | --- |
| Unit (`src/`) | 44 | configuration validation (paths, entries, units, origins, activation, sign-in methods, `apps = "signed-in"` needing a method, admin groups required for OIDC/proxy admin, OIDC prerequisites and `/auth/oidc/callback`, OIDC with an admin host refused, proxy prerequisites, CIDR parsing), umask, read-only trees keeping setgid, atomic symlink swap, version precedence, artifact matching, archive safety (traversal, absolute names, links, FIFOs, size/count/ratio limits, case collisions), no exec/setuid bits, content validation (root-absolute and missing references, native binaries, invalid wasm, mismatched `.gz`/`.br`), compression threshold, precompressed copies decode to originals, hidden and percent-encoded traversal paths refused, users file (Argon2, roles with `user` default, unknown roles ignored and reported, reload revokes users), roles from admin users/groups and user groups, post-login targets stay on the site, proxy headers ignored from untrusted peers, forwarded host/proto only from trusted peers, CSRF double submit and Origin, Basic decoding, OIDC ID-token checks (issuer, audience, authorized party, expiry, nonce) and unknown state, request activity, lock, state round-trip, every built-in app has an embedded WebP logo |
| Lifecycle (`tests/lifecycle.rs`, fake GitHub) | 15 | install/update with atomic publication and the HTTP serving check; checksum failures; unsafe and incompatible archives; interrupted downloads; rate limits; GitHub outage; disk-space budget; renamed artifacts; rollback/block/allow; pins incl. offline; retention incl. recently used releases; crash reconciliation; concurrent CLI processes; umask; idle activation (staged while busy, survives reconcile, applied after the quiet period, manual update bypasses it, rollback discards it, CLI never assumes idle); precompression backfill incl. interrupted runs |
| HTTP/admin (`tests/server.rs`) | 9 | launcher, status and health; embedded logos and fonts with their content types, unknown and `..` launcher paths 404; `/<app>` → `<app>/` → `<version>/` relative redirects with query; MIME types, `immutable` vs `no-cache`, ETag/304, Brotli and gzip responses; hidden, encoded-traversal, `current`, unknown-version and missing paths 404; POST 405; 503 installing page; admin disabled by default; `none` method still needs CSRF and same origin; Basic 401/200 and the user role refused by `/admin`, apps public by default; form login with `next`, off-site `next` ignored, cookie flags (`HttpOnly`, `SameSite=Lax`, `Path=/`), cross-site login and logout refused, logout revokes the session; `apps = "signed-in"`: page loads redirected to sign-in and back, other requests 401, health/sign-in/logos open, the user role reaches apps and status but gets 403 on `/admin` and its API, the admin role reaches both, `/auth/me`; proxy identities from trusted peers only, roles from groups, accounts without a role refused; admin host (with and without port, apps absent there, `/admin` absent elsewhere, sign-in on both, `X-Forwarded-Host` honoured from trusted proxies and ignored from other peers); OIDC against a fake provider (PKCE redirect, state bound to the starting browser, nonce, admin and user roles from groups, no role refused, forged state) |
| Compose (`tests/compose.rs`) | 1 scenario | host paths with spaces; first install; serving and hidden paths; form sign-in and **update from `/admin` without a restart** while the old release keeps serving; two simultaneous CLI updates (one install); disk exhaustion on a 1 MiB `WORK_DIR`; offline restart serving immediately; a different UID rejected by `doctor` without and accepted with the shared supplemental group, then serving and updating as that UID with group-inherited files |

## Live deployment against GitHub

The previous two-service deployment (directories under a path with a space, `RUN_UID=1000`,
`RUN_GID=10000`, `FILE_UMASK=0002`, setgid directories) was upgraded in place following
[install.md](install.md#from-the-two-service-layout-web--updater):

- All existing releases and state carried over; the server answered immediately and added
  precompressed copies to the seven existing releases in the background (about 40 s in total,
  EffectCraft's 58 MB module became a 12.7 MB Brotli response).
- The first check found all seven apps up to date.
- **Idle activation, end to end.** With `[apps.lightcraft] activation = "idle"`, `idle_after = "2m"`:
  LightCraft 0.2.1 was pinned and unpinned, the container restarted, and 0.4.0 was staged as
  pending. A browser tab was opened on `/lightcraft/` (0.2.1) and left open; 2 min 22 s after its
  last request the server logged "activated 0.4.0 while idle". The open tab still loaded its JS,
  WebAssembly and worker files from `/lightcraft/0.2.1/` (HTTP 200); a new visit went to 0.4.0.
- **One port, sign-in and roles** (`[auth] method = "form"`, `apps = "signed-in"`,
  `[admin] enabled = true`, users `admin` with the admin role and `demo` with the user role),
  in headless Chromium on the server through the LAN address `http://192.168.8.110:18080`:
  `/photocraft/` led to the sign-in page and, after signing in as `demo`, back to
  `/photocraft/0.5.0/`; the launcher showed all seven apps and "demo · user" without an Admin
  link; `admin/`, `admin/api/status` and an admin action answered 403 for that session; sign-out
  returned to the sign-in page. As `admin`: `/admin/` came back after sign-in, "Check" on
  PhotoCraft was accepted (202) and logged with the user name, the launcher showed the Admin
  link, an app opened, and sign-out from `/admin` returned to the sign-in page (the admin API then
  answered 401). No page errors. EffectCraft opened after sign-in with no failed requests; its
  web app manifest, which browsers fetch without cookies, first got 401 and is now left outside
  the sign-in gate. The previous demo configuration (sign-in settings under `[admin]`) was
  rejected with one message per moved key.
- Earlier, with `[admin] host = "admin.localhost"`, `/admin` answered only to requests for that
  host name (404 for `localhost` and the LAN address). Browsers resolve `*.localhost` to their own
  loopback address, so the name reached the server only from a browser on the server itself.

## Browser checks (headless Chromium, `http://localhost:18080`)

On the single-binary server all seven apps loaded from their versioned URLs with no failed
requests and no page errors; WebAssembly was delivered as `application/wasm` with gzip
(Chromium offers Brotli only over HTTPS).

Results from the earlier nginx-based deployment, same releases (the app files are byte-identical;
only the server changed):

| App | Renders | File opened | Edited / saved |
| --- | --- | --- | --- |
| PhotoCraft 0.5.0 | yes (WebGL2 backend) | PNG via file picker | inverted, saved (PSD download) |
| VectorCraft 0.7.0 | yes | SVG via drag and drop | not exercised |
| FilmCraft 0.4.0 | yes, demo project plays | not exercised | not exercised |
| LightCraft 0.4.0 | yes, demo library | not exercised | not exercised |
| PdfCraft 0.4.0 | yes | PDF via file picker | not exercised |
| EffectCraft 0.6.0 | yes, demo composition; service worker scoped to its release | not exercised | not exercised |
| DesignCraft 0.4.0 | yes | PDF placed via drag and drop into a new document | saved `.designcraft` (download) |

- **Open tab during an update**: PhotoCraft (0.3.0 → 0.5.0) and LightCraft (0.2.1 → 0.4.0 via
  idle activation) tabs kept working on their release; new visits got the new one.
- **Storage**: names used by the apps on one origin are listed in
  [apps.md](apps.md#browser-storage); `localhost` and `127.0.0.1` had separate storage.
- **Viewports**: launcher at 1280×800, 1400×1000 (light and dark color scheme) and 390×844; apps
  use desktop layouts.
- **Launcher**: all seven official logos decoded (256×256 WebP), Archivo and Geist Mono loaded,
  no console errors; with a substituted `status.json`, queued, installing, failed (error text
  shown as text, not HTML), pinned and logo-less custom-app cards rendered as intended; the first
  card link opened `/photocraft/0.5.0/`.

## Not verified

- HTTPS behind a real reverse proxy, Brotli delivery in browsers over HTTPS, WebGPU, browsers
  other than Chromium.
- OIDC against a real identity provider (tested against a fake provider implementing discovery
  and the token endpoint), including the `SameSite=Lax` session surviving the return from the
  provider in a real browser, and forward auth through a real Authentik outpost.
- Basic and proxy sign-in with protected apps in a real browser (covered by HTTP tests only);
  service workers under `apps = "signed-in"` (unavailable over plain HTTP, so EffectCraft's was
  not registered in the checks above).
- Saving in VectorCraft, FilmCraft, LightCraft, PdfCraft and EffectCraft; opening files in
  FilmCraft, LightCraft and EffectCraft; formats beyond PNG, SVG and PDF; re-running the file
  workflows on the new server (app files are unchanged, but the server is new).
- Whether an app is actually in use: idle activation only measures requests (see
  [operations](operations.md#activation-immediate-or-idle)).
- User-namespace remapping and Docker Desktop; hourly scheduling over long periods.
- Access from another device on the LAN: the LAN address was exercised from a browser on the
  server itself.
