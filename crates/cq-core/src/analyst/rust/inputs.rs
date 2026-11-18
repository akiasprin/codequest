use super::args::{Arguments, joined};
use crate::{model::*, ports::ContentRepository};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub(super) struct Inputs {
    files: BTreeMap<NativeValue, FileRevision>,
    gaps: Vec<Gap>,
    changed: bool,
    checked: usize,
}
impl Inputs {
    pub fn before(
        project: &Path,
        metadata: Option<&serde_json::Value>,
        repo: &dyn ContentRepository,
        budget: u64,
    ) -> Self {
        let mut this = Self {
            files: BTreeMap::new(),
            gaps: vec![],
            changed: false,
            checked: 0,
        };
        let mut paths = BTreeSet::new();
        discover(project, &mut paths, &mut this.gaps, 0);
        if let Some(packages) = metadata
            .and_then(|m| m.get("packages"))
            .and_then(|p| p.as_array())
        {
            for p in packages {
                if let Some(manifest) = p.get("manifest_path").and_then(|p| p.as_str()) {
                    paths.insert(PathBuf::from(manifest));
                    if let Some(dir) = Path::new(manifest).parent() {
                        discover(dir, &mut paths, &mut this.gaps, 0);
                    }
                }
            }
        }
        for path in paths {
            this.freeze(&path, "pre_build_discovery", repo, budget);
        }
        this
    }
    fn freeze(&mut self, path: &Path, origin: &str, repo: &dyn ContentRepository, budget: u64) {
        let key = NativeValue::new(path);
        if self.files.contains_key(&key) {
            return;
        }
        match repo.freeze(path, origin, budget) {
            Ok(file) => {
                self.files.insert(key, file);
            }
            Err(e) => self.gaps.push(Gap::new(
                "InputUnavailable",
                path.display().to_string(),
                e.to_string(),
            )),
        }
    }
    pub fn after(
        mut self,
        args: impl Iterator<Item = Arguments>,
        messages: &[serde_json::Value],
        repo: &dyn ContentRepository,
        budget: u64,
    ) -> SourceSnapshot {
        let mut paths = BTreeSet::new();
        for a in args {
            if let Some(root) = a.root {
                paths.insert(root.path());
            }
            if let (Some(dir), Some(name)) = (a.out_dir, a.name) {
                let dep = dir.join(format!("{name}{}.d", a.extra));
                match std::fs::read(&dep) {
                    Ok(bytes) => {
                        paths.insert(dep);
                        match dep_inputs(&bytes) {
                            Some(list) => {
                                for path in list {
                                    paths.insert(joined(&a.cwd, &NativeValue(path).path()));
                                }
                            }
                            None => self.gaps.push(Gap::new(
                                "DepInfoUnparsed",
                                name,
                                "dependency output did not match supported Make format",
                            )),
                        }
                    }
                    Err(e) => self.gaps.push(Gap::new(
                        "DepInfoUnavailable",
                        dep.display().to_string(),
                        e.to_string(),
                    )),
                }
            }
        }
        for m in messages
            .iter()
            .filter(|m| m["reason"] == "build-script-executed")
        {
            if let Some(dir) = m["out_dir"].as_str() {
                discover(Path::new(dir), &mut paths, &mut self.gaps, 0);
            }
        }
        // Retain the pre-build bytes. Comparing afterwards only detects some
        // changes; it never proves which bytes the compiler actually read.
        for (path, file) in &self.files {
            match std::fs::read(path.path()) {
                Ok(bytes) if BlobId::of(&bytes) == file.blob => self.checked += 1,
                _ => {
                    self.changed = true;
                    self.gaps.push(Gap::new(
                        "InputChanged",
                        path.display(),
                        "pre-build bytes retained; compilation binding needs review",
                    ));
                }
            }
        }
        for path in paths {
            self.freeze(&path, "post_build_dependency_hint", repo, budget);
        }
        self.gaps.push(Gap::new("InputScopePartial","snapshot","external file reads and transient changes are not exhaustively observed; fixed contents are not immutable compiler-input proof"));
        let binding = if self.changed {
            InputBinding::Changed
        } else if self.checked > 0 {
            InputBinding::PrePostChecked
        } else {
            InputBinding::Unknown
        };
        SourceSnapshot::seal(self.files.into_values().collect(), binding, self.gaps)
    }
}
fn discover(root: &Path, paths: &mut BTreeSet<PathBuf>, gaps: &mut Vec<Gap>, depth: usize) {
    if depth > 64 || paths.len() >= 100_000 {
        gaps.push(Gap::new(
            "DiscoveryLimit",
            root.display().to_string(),
            "depth 64 / 100000 files",
        ));
        return;
    }
    let entries = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(e) => {
            gaps.push(Gap::new(
                "DiscoveryUnavailable",
                root.display().to_string(),
                e.to_string(),
            ));
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            gaps.push(Gap::new("SymlinkExcluded",path.display().to_string(),"not recursively followed during source discovery; dep-info may supply concrete input"));
            continue;
        }
        let name = entry.file_name();
        if kind.is_dir() {
            if ![
                "target",
                ".git",
                "node_modules",
                ".codequest",
                ".codequest-tools",
            ]
            .iter()
            .any(|s| name == *s)
            {
                discover(&path, paths, gaps, depth + 1);
            }
        } else if path.extension().is_some_and(|s| s == "rs")
            || [
                "Cargo.toml",
                "Cargo.lock",
                "rust-toolchain",
                "rust-toolchain.toml",
                "config",
                "config.toml",
            ]
            .iter()
            .any(|s| name == *s)
        {
            paths.insert(path);
        }
    }
}
pub(super) fn dep_inputs(bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    let bytes = bytes
        .windows(2)
        .position(|w| w == b": " || w == b":\n")
        .map(|p| &bytes[p + 2..])?;
    let mut out = vec![];
    let mut word = vec![];
    let mut escape = false;
    for &b in bytes {
        if escape {
            if b != b'\n' && b != b'\r' {
                word.push(b);
            }
            escape = false;
            continue;
        }
        if b == b'\\' {
            escape = true;
            continue;
        }
        if b == b'\n' {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            break;
        }
        if b.is_ascii_whitespace() {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
        } else {
            word.push(b);
        }
    }
    if escape {
        return None;
    }
    if !word.is_empty() {
        out.push(word);
    }
    Some(out)
}
