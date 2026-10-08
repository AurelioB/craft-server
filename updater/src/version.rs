//! Release tags such as `v0.5.0` or `v0.1.1-rc.5`, ordered by semantic-version precedence.

use std::cmp::Ordering;

pub use semver::Version;

pub fn parse_tag(tag: &str) -> Option<Version> {
    let t = tag.trim();
    Version::parse(t.strip_prefix('v').unwrap_or(t)).ok()
}

/// Compare version strings; unparsable strings sort before every version.
pub fn cmp_text(a: &str, b: &str) -> Ordering {
    match (parse_tag(a), parse_tag(b)) {
        (Some(x), Some(y)) => x.cmp_precedence(&y),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prerelease_suffix_sorts_before_release() {
        let rc = parse_tag("v0.1.1-rc.5").unwrap();
        assert!(!rc.pre.is_empty());
        assert!(rc < parse_tag("v0.1.1").unwrap());
        assert_eq!(cmp_text("0.10.0", "0.9.0"), Ordering::Greater);
        assert!(parse_tag("nightly").is_none());
        assert_eq!(parse_tag("v0.5.0").unwrap().to_string(), "0.5.0");
        assert_eq!(cmp_text("1.0.0+build.1", "1.0.0+build.2"), Ordering::Equal);
    }
}
