# Applications

Survey of the official releases, 8 October 2026. The built-in manifest is
`updater/src/manifest.toml`.

| App | Repository | Web artifact | Version checked | Expanded | Notes |
| --- | --- | --- | --- | --- | --- |
| PhotoCraft | `storytold/photocraft` | `photocraft-web-{v}.zip` | 0.5.0 | 22 MB | content-hashed asset names |
| VectorCraft | `storytold/vectorcraft` | `vectorcraft-web-{v}.zip` | 0.7.0 | 48 MB | content-hashed asset names |
| FilmCraft | `storytold/filmcraft` | `filmcraft-web-{v}.zip` | 0.4.0 | 40 MB | assets loaded with `?v=<build>` |
| LightCraft | `storytold/lightcraft` | `lightcraft-web-{v}.zip` | 0.4.0 | 37 MB published (170 MB archive contents) | archive also holds a Cargo target dir; precompressed `.gz`/`.br` copies |
| PdfCraft | `storytold/printcraft` | `pdfcraft-web-{v}.zip` (≤ 0.2.1: `printcraft-web-{v}.zip`) | 0.4.0 | 34 MB | repository and artifact names differ |
| EffectCraft | `storytold/effectcraft` | `effectcraft-web-{v}.zip` | 0.6.0 | 57 MB | service worker, web manifest |
| DesignCraft | `storytold/designcraft` | `designcraft-web-{v}.zip` | 0.4.0 | 41 MB | content-hashed asset names |

Common to all seven:

- Each release also publishes desktop packages and `SHA256SUMS.txt`; the GitHub API reports a
  SHA-256 digest for every asset. Both matched the downloaded web archives.
- The archive holds one top-level directory `<artifact>-web-<version>/` with `index.html`,
  wasm-bindgen glue and one WebAssembly module, plus `HOSTING.md`, `_headers` and `.htaccess`.
- `index.html` uses relative URLs only, so every app works under a nested path such as
  `/photocraft/0.5.0/`.
- Requirements from upstream `HOSTING.md`: `.wasm` as `application/wasm`, `.js` as
  `text/javascript`, gzip or Brotli compression, HTTPS (or localhost) for WebGPU, clipboard and
  storage. No cross-origin isolation headers are needed.
- Some releases reuse the same asset names across versions (EffectCraft, LightCraft). Serving each
  release from its own versioned directory makes long-lived caching safe for all apps; entry
  pages, `sw.js` and manifests are still revalidated.
- Tag `v0.1.1-rc.5` of PhotoCraft is not flagged as a pre-release on GitHub; the updater treats
  semver pre-release suffixes as pre-releases regardless of the flag.

## LightCraft archive contents

`lightcraft-web-0.4.0.zip` contains 572 entries, 549 of which are a Cargo target directory
(`build/`, `deps/`, `.fingerprint/`, `examples/`, `incremental/`, `.cargo-*lock`) with native
build scripts, `.so` and `.rlib` files. The site never references them. The manifest excludes
these paths (documented configuration, not patching); without the exclusion the updater rejects
the release because it contains native executables. The remaining files, including the
precompressed copies, are published unchanged; shipped `.gz`/`.br` copies are verified to
decompress to their originals because the server sends them in place of the originals.

## Service workers

EffectCraft registers `sw.js` with scope `./`, i.e. the release directory
(`/effectcraft/0.6.0/`). Each release therefore has its own service-worker scope; a newer release
does not take over tabs of an older one. On activation it deletes caches of other EffectCraft
versions; tabs still on an older release fall back to the network, which keeps serving retained
releases.

## Browser storage

All apps on one host share an origin. Observed after using all seven on one origin:

| Kind | Names |
| --- | --- |
| `localStorage` | `photocraft.preferences`, `pdfcraft`, `egui_memory_ron` (PdfCraft) |
| IndexedDB | `effectcraft-handles` |
| OPFS root | `effectcraft/`, `effectcraft-cache/`, `library/`, `thumbs/` (LightCraft), `recovery/` |
| Cache Storage | `effectcraft-<build>` |

No collision was observed, but `library/`, `thumbs/`, `recovery/` (strings present in both
FilmCraft and VectorCraft) and `egui_memory_ron` are generic names, so future versions or
additional apps could collide. Give an app its own origin with the `origin` setting
([operations](operations.md#separate-origins)) if that matters for your deployment.

LightCraft keeps its photo library in browser storage and allows one tab at a time per origin
(upstream behaviour). Browsers may evict storage that is not persisted; the apps provide their
own export and backup functions.
