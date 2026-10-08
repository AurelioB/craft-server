//! Exclusion patterns: `*` matches any run of characters including `/`, `?` one character,
//! and `dir/**` also matches `dir` itself.

fn matches(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let (mut star_p, mut star_t) = (None, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star_p = Some(p);
            star_t = t;
            p += 1;
        } else if let Some(sp) = star_p {
            p = sp + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

pub fn is_excluded(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pat| {
        matches(pat.as_bytes(), path.as_bytes())
            || pat.strip_suffix("/**").is_some_and(|dir| dir == path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_patterns_cover_the_directory_and_descendants() {
        let pats = vec![
            "build/**".to_string(),
            ".cargo-lock".to_string(),
            "*.rlib".to_string(),
        ];
        assert!(is_excluded("build", &pats));
        assert!(is_excluded("build/x/y.so", &pats));
        assert!(is_excluded(".cargo-lock", &pats));
        assert!(is_excluded("deps/libfoo.rlib", &pats));
        assert!(!is_excluded("builder.js", &pats));
        assert!(!is_excluded("index.html", &pats));
    }
}
