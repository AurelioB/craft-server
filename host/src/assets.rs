//! Launcher assets compiled into the binary: pages, script, styles, the official app logos and
//! the fonts used by https://getartcraft.com (Archivo and Geist Mono, SIL Open Font License).

pub struct Asset {
    pub bytes: &'static [u8],
    pub content_type: &'static str,
}

macro_rules! asset {
    ($path:literal, $ct:literal) => {
        Asset {
            bytes: include_bytes!(concat!("../assets/", $path)),
            content_type: $ct,
        }
    };
}

/// `(path under /launcher/, asset)`.
const LAUNCHER: &[(&str, Asset)] = &[
    (
        "launcher.js",
        asset!("launcher.js", "text/javascript; charset=utf-8"),
    ),
    (
        "launcher.css",
        asset!("launcher.css", "text/css; charset=utf-8"),
    ),
    (
        "installing.html",
        asset!("installing.html", "text/html; charset=utf-8"),
    ),
    (
        "icons/artcraft.svg",
        asset!("icons/artcraft.svg", "image/svg+xml"),
    ),
    (
        "icons/photocraft.webp",
        asset!("icons/photocraft.webp", "image/webp"),
    ),
    (
        "icons/vectorcraft.webp",
        asset!("icons/vectorcraft.webp", "image/webp"),
    ),
    (
        "icons/filmcraft.webp",
        asset!("icons/filmcraft.webp", "image/webp"),
    ),
    (
        "icons/lightcraft.webp",
        asset!("icons/lightcraft.webp", "image/webp"),
    ),
    (
        "icons/pdfcraft.webp",
        asset!("icons/pdfcraft.webp", "image/webp"),
    ),
    (
        "icons/effectcraft.webp",
        asset!("icons/effectcraft.webp", "image/webp"),
    ),
    (
        "icons/designcraft.webp",
        asset!("icons/designcraft.webp", "image/webp"),
    ),
    (
        "fonts/archivo-latin.woff2",
        asset!("fonts/archivo-latin.woff2", "font/woff2"),
    ),
    (
        "fonts/geist-mono-latin.woff2",
        asset!("fonts/geist-mono-latin.woff2", "font/woff2"),
    ),
    (
        "fonts/Archivo-OFL.txt",
        asset!("fonts/Archivo-OFL.txt", "text/plain; charset=utf-8"),
    ),
    (
        "fonts/GeistMono-OFL.txt",
        asset!("fonts/GeistMono-OFL.txt", "text/plain; charset=utf-8"),
    ),
];

pub fn launcher(path: &str) -> Option<&'static Asset> {
    LAUNCHER.iter().find(|(p, _)| *p == path).map(|(_, a)| a)
}

/// Launcher URL (relative to the site root) of the official logo for a built-in app.
pub fn logo(app_id: &str) -> Option<String> {
    let path = format!("icons/{app_id}.webp");
    launcher(&path).map(|_| format!("launcher/{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_app_has_a_logo() {
        let manifest: toml::Table = toml::from_str(crate::config::MANIFEST).unwrap();
        for id in manifest["apps"].as_table().unwrap().keys() {
            let url = logo(id).unwrap_or_else(|| panic!("no logo for {id}"));
            let asset = launcher(url.strip_prefix("launcher/").unwrap()).unwrap();
            assert!(asset.bytes.starts_with(b"RIFF"), "{id} logo is WebP");
        }
        assert!(logo("examplecraft").is_none());
    }
}
