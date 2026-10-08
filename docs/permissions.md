# Users, groups and permissions

## What the identity governs

`RUN_UID`/`RUN_GID` are the process identity of the container (Compose `user:`). They apply to
the files it creates on the host: releases, state, cache, downloads and logs. They do not affect
documents a visitor saves on their own computer: the apps save through browser downloads,
user-approved local files, or browser storage.

## Settings

| Setting | Effect |
| --- | --- |
| `RUN_UID`, `RUN_GID` | Numeric process identity (non-root) |
| `SUPPLEMENTAL_GID` | One extra group (Compose `group_add:`), e.g. a group shared with other tools |
| `FILE_UMASK` | Umask applied to everything the server writes (default `0022`) |

Identities are numeric; names are not resolved. The process refuses to modify anything as root
unless `CRAFT_ALLOW_ROOT=1` is set. Need more groups? Add them in a `compose.override.yaml`
under `group_add:`.

## Rules the server follows

- Every file is created with mode `0666 & ~umask` and every directory with `0777 & ~umask`.
- Archive ownership, setuid/setgid bits and executable bits are never restored.
- Published releases are made read-only (`a-w`) after publication; existing setgid bits are kept.
  Precompressed copies for releases from older versions are added by temporarily granting the
  owner write permission on the release directories.
- Directories inherit a parent's setgid bit (the kernel does this); the server never clears it.
- Ownership is never changed.
- The root filesystem of the container is read-only; `/tmp` is a small tmpfs.
- Only the HTTP server is reachable from outside. It serves release files read from
  `DATA_DIR/<release_dir>/<version>/` and never serves configuration, state, cache, staging or
  hidden files.

## Sharing directories with another identity

When other tools must read or manage the directories, use a shared group:

```sh
sudo install -d -o 1000 -g 10000 -m 2770 /srv/craft-apps/data   # setgid: new files get group 10000
# .env: RUN_UID=1000  RUN_GID=10000  FILE_UMASK=0007
```

Running as a different user that only shares the group also works:
`RUN_UID=54321 RUN_GID=54321 SUPPLEMENTAL_GID=10000` with group-writable (`2770`) directories and
a umask that keeps group write (`0007` or `0002`). `doctor` probes read and write access with the
actual identity and prints preparation commands for directories it cannot use.

## Host-specific behaviour

- **User-namespace remapping (`userns-remap`, rootless Docker).** Container IDs map to subordinate
  host IDs (for example container `1000` → host `101000`). Prepare directories for the mapped
  host IDs. `doctor` reports the identity seen inside the container and warns when it differs
  from `RUN_UID`/`RUN_GID`.
- **Docker Desktop (macOS/Windows).** Bind mounts go through a file-sharing layer that does not
  preserve Unix ownership and permissions. Permission checks are best effort there; use a Linux
  host for production.
- **NFS/SMB-backed `STATE_DIR`.** The operation lock uses `flock`; prefer a local filesystem for
  `STATE_DIR`. `DATA_DIR` must support atomic rename and symlinks.
