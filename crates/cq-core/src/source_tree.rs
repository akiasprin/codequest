use crate::{Error, Result, model::*};
use std::{collections::BTreeMap, path::Path};
/// Relevant working-tree observations, independent of semantic proof.
pub fn manifest(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut entries = BTreeMap::new();
    walk(root, &mut entries, 0)?;
    Ok(entries)
}
fn walk(root: &Path, out: &mut BTreeMap<String, String>, depth: usize) -> Result<()> {
    if depth > 48 || out.len() > 100000 {
        return Err(Error::new("DiscoveryLimit", "working tree scope limit"));
    }
    for e in std::fs::read_dir(root)? {
        let e = e?;
        let ty = e.file_type()?;
        let path = e.path();
        if ty.is_symlink() {
            continue;
        }
        if ty.is_dir() {
            if ![
                ".git",
                "target",
                "node_modules",
                ".codequest",
                ".codequest-tools",
                "vendor",
            ]
            .iter()
            .any(|s| e.file_name() == *s)
            {
                walk(&path, out, depth + 1)?;
            }
        } else if path.extension().and_then(|s| s.to_str()).is_some_and(|s| {
            [
                "rs", "go", "c", "cc", "cpp", "cxx", "h", "hpp", "hxx", "ts", "tsx", "mts", "cts",
                "js", "jsx", "mjs", "cjs", "md", "rst", "txt", "pdf", "toml", "yaml", "yml",
                "json", "mod", "sum", "work", "lock",
            ]
            .contains(&s)
        }) || ["README", "rust-toolchain", "config", "Makefile"]
            .iter()
            .any(|name| e.file_name() == *name)
        {
            let meta = e.metadata()?;
            if meta.len() > 64 * 1024 * 1024 {
                return Err(Error::new("ContentLimit", path.display().to_string()));
            }
            out.insert(
                path.to_string_lossy().into_owned(),
                digest(&std::fs::read(&path)?),
            );
        }
    }
    Ok(())
}

/// Include resolution depends on absent names as well as previously read files.
pub fn directory_digest(path: &Path) -> Result<String> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(digest(b"absent directory"));
        }
        Err(e) => return Err(e.into()),
    };
    let mut names = vec![];
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = if kind.is_symlink() {
            Some(NativeValue::new(std::fs::read_link(entry.path())?))
        } else {
            None
        };
        names.push((NativeValue::new(entry.file_name()), kind.is_dir(), target));
    }
    names.sort();
    Ok(digest(&serde_json::to_vec(&names)?))
}
