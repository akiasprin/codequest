use crate::model::*;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default)]
pub(super) struct Arguments {
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub root: Option<NativeValue>,
    pub types: Vec<String>,
    pub cfg: Vec<String>,
    pub check_cfg: Vec<String>,
    pub test: bool,
    pub edition: Option<String>,
    pub target: Option<String>,
    pub out_dir: Option<PathBuf>,
    pub extra: String,
    pub externs: Vec<(String, NativeValue)>,
    pub probe: bool,
    pub gaps: Vec<Gap>,
    pub raw: Vec<NativeValue>,
    pub cfg_probe_args: Vec<String>,
}
pub(super) fn joined(cwd: &Path, path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::new();
    for p in path.components() {
        match p {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            p => out.push(p),
        }
    }
    out
}
pub(super) fn inspect(inv: &CompilerInvocation) -> Arguments {
    let mut result = Arguments::default();
    let Some(Envelope {
        event: Event::InvocationStarted { args, cwd, .. },
        ..
    }) = &inv.started
    else {
        result
            .gaps
            .push(Gap::new("MissingStart", &inv.id, "no argument observation"));
        return result;
    };
    result.cwd = cwd.path();
    result.raw = args.clone();
    let files: HashMap<_, _> = inv
        .argument_files
        .iter()
        .filter_map(|e| {
            if let Event::ArgumentFileObserved { path, contents, .. } = &e.event {
                Some((path.clone(), contents))
            } else {
                None
            }
        })
        .collect();
    let mut expanded = Vec::new();
    for arg in args {
        if arg.0.starts_with(b"@") {
            let path = NativeValue(arg.0[1..].to_vec());
            if let Some(Some(bytes)) = files.get(&path)
                && let Some(text) = bytes.text()
            {
                for line in text.lines() {
                    if line.starts_with('@') {
                        result.gaps.push(Gap::new(
                            "NestedArgumentFile",
                            &inv.id,
                            "nested response file is not expanded",
                        ));
                    }
                    expanded.push(NativeValue::from(line));
                }
                continue;
            }
            result
                .gaps
                .push(Gap::new("ArgumentFileUnavailable", &inv.id, path.display()));
        }
        expanded.push(arg.clone());
    }
    let mut i = 0;
    while i < expanded.len() {
        let arg = &expanded[i];
        let Some(text) = arg.text() else {
            result.gaps.push(Gap::new(
                "NonUnicodeOption",
                &inv.id,
                "raw OS bytes retained; semantic option not decoded",
            ));
            i += 1;
            continue;
        };
        let (flag, inline) = text
            .split_once('=')
            .map_or((text, None), |(a, b)| (a, Some(b.to_owned())));
        if ["--version", "-V", "-vV", "--print", "--help", "-h"].contains(&flag) {
            result.probe = true;
        }
        let consumes = [
            "--crate-name",
            "--crate-type",
            "--edition",
            "--cfg",
            "--check-cfg",
            "--target",
            "--out-dir",
            "--extern",
            "--emit",
            "--sysroot",
            "--print",
            "--cap-lints",
            "--force-warn",
            "--error-format",
            "--json",
            "--diagnostic-width",
            "--color",
            "--remap-path-prefix",
            "-C",
            "--codegen",
            "-L",
            "-l",
            "-A",
            "-W",
            "-D",
            "-F",
        ];
        if consumes.contains(&flag) {
            let value = if let Some(value) = inline {
                Some(NativeValue::from(value.as_str()))
            } else {
                i += 1;
                expanded.get(i).cloned()
            };
            if let Some(value) = value {
                let v = value.text().unwrap_or("");
                match flag {
                    "--crate-name" => result.name = value.text().map(str::to_owned),
                    "--crate-type" => result.types.extend(v.split(',').map(str::to_owned)),
                    "--edition" => result.edition = value.text().map(str::to_owned),
                    "--cfg" => result.cfg.push(v.into()),
                    "--check-cfg" => result.check_cfg.push(v.into()),
                    "--target" => result.target = value.text().map(str::to_owned),
                    "--out-dir" => result.out_dir = Some(joined(&result.cwd, &value.path())),
                    "--extern" => {
                        if let Some((alias, path)) = v.split_once('=') {
                            result.externs.push((
                                alias.trim_start_matches("priv:").into(),
                                NativeValue::new(joined(&result.cwd, Path::new(path))),
                            ));
                        } else {
                            result.gaps.push(Gap::new("ExternWithoutPath", &inv.id, v));
                        }
                    }
                    "-C" | "--codegen" => {
                        if let Some(extra) = v.strip_prefix("extra-filename=") {
                            result.extra = extra.into();
                        }
                    }
                    _ => {}
                }
                if ["--target", "--sysroot", "--cfg", "-C", "--codegen"].contains(&flag) {
                    result.cfg_probe_args.extend([flag.into(), value.display()]);
                }
                if value.text().is_none() {
                    result
                        .gaps
                        .push(Gap::new("NonUnicodeOption", &inv.id, flag));
                }
            } else {
                result
                    .gaps
                    .push(Gap::new("MissingOptionValue", &inv.id, flag));
            }
        } else if flag == "--test" {
            result.test = true;
            result.cfg_probe_args.push("--test".into());
        } else if text.starts_with("-C") && text.len() > 2 {
            if let Some(extra) = text[2..].strip_prefix("extra-filename=") {
                result.extra = extra.into();
            }
            result.cfg_probe_args.push(text.into());
        } else if ["-g", "-O", "-v", "-V", "-vV", "--version", "--help", "-h"].contains(&text)
            || ["-L", "-l", "-A", "-W", "-D", "-F"]
                .iter()
                .any(|s| text.starts_with(s) && text.len() > 2)
        {
        } else if text.starts_with('-') || text.starts_with('@') {
            result
                .gaps
                .push(Gap::new("UnknownCompilerOption", &inv.id, text));
        } else {
            let root = NativeValue::new(joined(&result.cwd, &arg.path()));
            if result.root.replace(root).is_some() {
                result.gaps.push(Gap::new(
                    "AmbiguousRoot",
                    &inv.id,
                    "multiple positional compiler inputs",
                ));
            }
        }
        i += 1;
    }
    result
}
pub(super) fn output_matches(args: &Arguments, path: &NativeValue) -> bool {
    let (Some(dir), Some(name)) = (&args.out_dir, &args.name) else {
        return false;
    };
    let p = path.path();
    if p.parent() != Some(dir.as_path()) {
        return false;
    }
    let stem = format!("{name}{}", args.extra);
    let Some(file) = p.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    file == stem
        || ["rlib", "rmeta", "so", "dylib", "a"]
            .iter()
            .any(|ext| file == format!("lib{stem}.{ext}"))
        || ["exe", "dll"]
            .iter()
            .any(|ext| file == format!("{stem}.{ext}"))
}
