# Users, groups and permissions

## What the identities govern

The UID/GID settings apply to the two server processes and the files they create on the host:
releases, status, state, cache, downloads and logs. They do not affect documents a visitor saves
on their own computer: the apps save through browser downloads, user-approved local files, or
browser storage.

## Settings

| Setting | Effect |
| --- | --- |
| `RUN_UID`, `RUN_GID` | Default numeric identity of both services (Compose `user:`) |
| `WEB_UID`, `WEB_GID` | Override for the web server |
| `UPDATER_UID`, `UPDATER_GID` | Override for the updater |
| `WEB_SUPPLEMENTAL_GID`, `UPDATER_SUPPLEMENTAL_GID` | One extra group per service (Compose `group_add:`) |
| `FILE_UMASK` | Umask applied by the updater to everything it writes (default `0022`) |

Identities are numeric; names are not resolved. Both services run non-root by default; the
updater refuses to modify anything as root unless `CRAFT_ALLOW_ROOT=1` is set. Need more than one
extra group? Add them in a `compose.override.yaml` under `group_add:`.

## Rules the updater follows

- Every file is created with mode `0666 & ~umask` and every directory with `0777 & ~umask`.
- Archive ownership, setuid/setgid bits and executable bits are never restored.
- Published releases are made read-only (`a-w`) after publication; existing setgid bits are kept.
- Directories inherit a parent's setgid bit (the kernel does this); the updater never clears it.
- Ownership is never changed, recursively or otherwise.
- The web server mounts only `DATA_DIR`, read-only, and cannot modify published content. The
  configuration, token, state, cache and work directories are not mounted into it.

## Common layouts

**Same identity for both services (simplest).** `RUN_UID`/`RUN_GID` only; any umask that keeps
owner read works.

The web server also reads `server/` and `launcher/` from this checkout (bind-mounted read-only).
When the web identity differs from the checkout's owner, those directories must be readable and
searchable by it. A checkout made under a restrictive umask (e.g. `0077`) is not; fix it once
with `chmod -R a+rX server launcher` (Git does not record read permissions, so repeat after a
fresh clone with the same umask).

**Separate identities sharing a group.** For example, the updater as `1000:10000` and the web
server as `101:101` with `WEB_SUPPLEMENTAL_GID=10000`:

```sh
sudo install -d -o 1000 -g 10000 -m 2750 /srv/craft-apps/data   # setgid: new files get group 10000
# .env: FILE_UMASK=0027  (group read, no access for others)
```

**Separate identities without a shared group.** Works only when files are world-readable
(`FILE_UMASK` `0022` or `0002`) and `DATA_DIR` is world-traversable.

`doctor` computes, from the umask, the setgid bit of `DATA_DIR` and the configured web identity,
whether the web server will be able to read what the updater publishes, and scans existing
published content for unreadable paths. It reports incompatibilities as `FAIL` with a suggested
fix.

## Host-specific behaviour

- **User-namespace remapping (`userns-remap`, rootless Docker).** Container IDs map to subordinate
  host IDs (for example container `1000` → host `101000`). Prepare directories for the mapped
  host IDs. `doctor` reports the identity seen inside the container.
- **Docker Desktop (macOS/Windows).** Bind mounts go through a file-sharing layer that does not
  preserve Unix ownership and permissions; files may appear owned by the container user
  regardless of host ownership. Permission checks are best effort there; keep the project on a
  Linux host for production.
- **NFS/SMB-backed `STATE_DIR`.** The operation lock uses `flock`; prefer a local filesystem for
  `STATE_DIR`. `DATA_DIR` must support atomic rename and symlinks.
