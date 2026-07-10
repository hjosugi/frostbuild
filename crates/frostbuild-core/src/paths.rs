use anyhow::{bail, Result};

/// Validate and normalize a workspace-relative path from a manifest.
///
/// Rules: non-empty, relative, forward slashes only, no `.`/`..` components.
/// Returns the normalized form (leading `./` stripped).
pub fn validate_rel_path(raw: &str) -> Result<String> {
    if raw.is_empty() {
        bail!("empty path");
    }
    if raw.contains('\\') {
        bail!("path {raw:?} must use forward slashes");
    }
    if raw.starts_with('/') {
        bail!("path {raw:?} must be workspace-relative, not absolute");
    }
    let mut parts = Vec::new();
    for part in raw.split('/') {
        match part {
            "" => bail!("path {raw:?} has an empty component"),
            "." => continue,
            ".." => bail!("path {raw:?} must not escape the workspace with `..`"),
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        bail!("path {raw:?} does not name a file");
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_and_normalizes() {
        assert_eq!(validate_rel_path("src/main.c").unwrap(), "src/main.c");
        assert_eq!(validate_rel_path("./src/main.c").unwrap(), "src/main.c");
    }

    #[test]
    fn rejects_bad_paths() {
        assert!(validate_rel_path("").is_err());
        assert!(validate_rel_path("/etc/passwd").is_err());
        assert!(validate_rel_path("../escape.c").is_err());
        assert!(validate_rel_path("a//b").is_err());
        assert!(validate_rel_path("a\\b").is_err());
        assert!(validate_rel_path(".").is_err());
    }
}
