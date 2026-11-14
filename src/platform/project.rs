//! Project identity: a stable per-directory identifier derived from the
//! canonicalized working directory, with an explicit override via the
//! FLATCAKE_PROJECT environment variable. No file contents are read.

use std::path::Path;

/// Derive a project id from `dir` (falls back to the current directory when
/// `dir` is None). Deterministic across runs on the same machine.
pub fn project_id(dir: Option<&Path>) -> String {
    if let Ok(explicit) = std::env::var("FLATCAKE_PROJECT") {
        if !explicit.trim().is_empty() {
            return format!("env:{}", explicit.trim());
        }
    }
    let base = match dir {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir().unwrap_or_default(),
    };
    let canon = base.canonicalize().unwrap_or(base);
    let s = canon.to_string_lossy().replace('\\', "/");
    format!("dir:{}", crate::util::fnv1a64(s.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_per_directory() {
        let a = project_id(Some(Path::new("src")));
        let b = project_id(Some(Path::new("src")));
        assert_eq!(a, b);
        let c = project_id(Some(Path::new("tests")));
        assert_ne!(a, c);
    }
}
