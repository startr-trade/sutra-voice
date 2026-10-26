//! Secrets are references, never literals: a setting that holds a credential names WHERE the
//! credential is (`env:NAME`, `file:/path`), and a literal value is refused at boot.

/// Resolve a reference through `env` (the process environment in production) and the file
/// system. `what` names the setting in the error.
pub fn resolve(
    what: &str,
    reference: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let reference = reference.trim();
    let value = if let Some(name) = reference.strip_prefix("env:") {
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("{what}: 'env:' names no variable"));
        }
        env(name).ok_or_else(|| format!("{what}: environment variable {name} is not set"))?
    } else if let Some(path) = reference.strip_prefix("file:") {
        let path = path.trim();
        if path.is_empty() {
            return Err(format!("{what}: 'file:' names no file"));
        }
        std::fs::read_to_string(path).map_err(|e| format!("{what}: cannot read {path}: {e}"))?
    } else {
        return Err(format!(
            "{what} must be a reference (env:NAME or file:/path), not a literal secret"
        ));
    };
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(format!("{what}: the referenced value is empty"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        (name == "LK_SECRET").then(|| "s3cret\n".to_string())
    }

    #[test]
    fn references_resolve_and_literals_are_refused() {
        assert_eq!(resolve("KEY", "env:LK_SECRET", env).unwrap(), "s3cret");
        assert!(resolve("KEY", "env:MISSING", env)
            .unwrap_err()
            .contains("MISSING"));
        let literal = resolve("SUTRA_WORKER_API_SECRET", "devsecret", env).unwrap_err();
        assert!(literal.contains("not a literal"), "{literal}");
        assert!(
            !literal.contains("devsecret"),
            "the error never echoes the value"
        );
        assert!(resolve("KEY", "env:", env).is_err());
        let dir = std::env::temp_dir().join(format!("sutra-ref-{}", std::process::id()));
        std::fs::write(&dir, "from-file\n").unwrap();
        assert_eq!(
            resolve("KEY", &format!("file:{}", dir.display()), env).unwrap(),
            "from-file"
        );
        std::fs::write(&dir, "  \n").unwrap();
        assert!(resolve("KEY", &format!("file:{}", dir.display()), env).is_err());
        let _ = std::fs::remove_file(&dir);
    }
}
