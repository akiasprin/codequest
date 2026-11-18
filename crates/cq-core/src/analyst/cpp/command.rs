//! Effective replay language and response inputs belong to the compilation
//! command, not to the requested repository language or the caller's cwd.
use crate::{Error, Result, model::*};
use std::path::{Path, PathBuf};
pub struct Replay {
    pub language: Language,
    pub arguments: Vec<String>,
    pub response_inputs: Vec<PathBuf>,
}
fn expand(
    args: &[String],
    cwd: &Path,
    depth: usize,
    inputs: &mut Vec<PathBuf>,
) -> Result<Vec<String>> {
    if depth > 8 {
        return Err(Error::new(
            "ResponseFileLimit",
            "response-file nesting exceeds 8",
        ));
    }
    let mut out = vec![];
    for arg in args {
        if let Some(name) = arg.strip_prefix('@') {
            let path = cwd.join(name).canonicalize()?;
            if std::fs::metadata(&path)?.len() > 8 * 1024 * 1024 {
                return Err(Error::new(
                    "ResponseFileLimit",
                    "response file exceeds 8 MiB",
                ));
            }
            let text = std::fs::read_to_string(&path)?;
            let nested = shlex::split(&text)
                .ok_or_else(|| Error::new("InvalidResponseFile", path.display().to_string()))?;
            inputs.push(path);
            out.extend(expand(&nested, cwd, depth + 1, inputs)?);
        } else {
            out.push(arg.clone());
        }
        if out.len() > 100_000 {
            return Err(Error::new(
                "ResponseFileLimit",
                "expanded arguments exceed 100000",
            ));
        }
    }
    Ok(out)
}
pub fn replay(args: &[String], cwd: &Path, file: &Path) -> Result<Replay> {
    let mut response_inputs = vec![];
    let expanded = expand(args, cwd, 0, &mut response_inputs)?;
    let mut driver_at = 0;
    while expanded.get(driver_at).is_some_and(|a| {
        matches!(
            Path::new(a).file_name().and_then(|p| p.to_str()),
            Some("ccache" | "sccache" | "distcc")
        )
    }) {
        driver_at += 1;
    }
    let driver = expanded.get(driver_at).ok_or_else(|| {
        Error::new(
            "UnsupportedCompilerDriver",
            "missing compiler after launcher",
        )
    })?;
    let name = Path::new(driver)
        .file_name()
        .and_then(|p| p.to_str())
        .unwrap_or("");
    let known = name.contains("clang")
        || name.contains("gcc")
        || name.contains("g++")
        || matches!(name, "cc" | "c++" | "CC");
    if !known || name.contains("clang-cl") {
        return Err(Error::new(
            "UnsupportedCompilerDriver",
            format!("cannot attest replay language for {driver}; supply a compiler command"),
        ));
    }
    let mut cxx = name.contains("++") || name == "CC";
    for (i, arg) in expanded.iter().enumerate().skip(driver_at + 1) {
        if let Some(mode) = arg.strip_prefix("--driver-mode=") {
            cxx = match mode {
                "g++" => true,
                "gcc" | "cpp" => false,
                _ => {
                    return Err(Error::new(
                        "UnsupportedCompilerDriver",
                        format!("unsupported driver mode {mode}"),
                    ));
                }
            };
        }
        let directory = if arg == "-working-directory" {
            Some(
                expanded
                    .get(i + 1)
                    .ok_or_else(|| {
                        Error::new(
                            "InvalidCompilerArguments",
                            "-working-directory needs a directory",
                        )
                    })?
                    .as_str(),
            )
        } else {
            arg.strip_prefix("-working-directory=")
        };
        if let Some(directory) = directory
            && cwd.join(directory).canonicalize()? != cwd
        {
            return Err(Error::new(
                "UnsupportedCompilerDirectory",
                "working-directory override differs from compilation database directory",
            ));
        }
    }
    let mut forced = None;
    let mut selected = None;
    let mut skip = false;
    for (i, arg) in expanded.iter().enumerate().skip(driver_at + 1) {
        if skip {
            skip = false;
            continue;
        }
        if arg == "-x" {
            forced = Some(
                expanded
                    .get(i + 1)
                    .ok_or_else(|| Error::new("InvalidCompilerArguments", "-x needs a language"))?
                    .as_str(),
            );
            skip = true;
            continue;
        }
        if let Some(lang) = arg.strip_prefix("-x").filter(|s| !s.is_empty()) {
            forced = Some(lang);
            continue;
        }
        if [
            "-o",
            "-MF",
            "-MT",
            "-MQ",
            "-I",
            "-isystem",
            "-iquote",
            "-idirafter",
            "-include",
            "-imacros",
            "-include-pch",
            "--sysroot",
            "-isysroot",
            "-target",
            "-working-directory",
        ]
        .contains(&arg.as_str())
        {
            skip = true;
            continue;
        }
        if !arg.starts_with('-') && cwd.join(arg).canonicalize().ok().as_deref() == Some(file) {
            selected = Some(forced);
            break;
        }
    }
    let forced = selected.ok_or_else(|| {
        Error::new(
            "InvalidCompilationDatabase",
            "command does not contain the selected input file",
        )
    })?;
    let language = match forced {
        Some("c" | "c-header" | "cpp-output") => Language::C,
        Some("c++" | "c++-header" | "c++-cpp-output") => Language::Cpp,
        None | Some("none") => {
            if cxx {
                Language::Cpp
            } else {
                match file.extension().and_then(|p| p.to_str()) {
                    Some("c" | "h" | "i") => Language::C,
                    Some("cc" | "cpp" | "cxx" | "c++" | "C" | "ii" | "hpp" | "hxx" | "hh") => {
                        Language::Cpp
                    }
                    _ => {
                        return Err(Error::new(
                            "UnsupportedCompilerLanguage",
                            "selected input needs an explicit -x",
                        ));
                    }
                }
            }
        }
        Some(other) => return Err(Error::new("UnsupportedCompilerLanguage", other)),
    };
    let mut analysis = vec![];
    let mut skip = false;
    for arg in expanded.iter().skip(driver_at + 1) {
        if skip {
            skip = false;
            continue;
        }
        if ["-o", "-MF", "-MT", "-MQ", "-x", "-working-directory"].contains(&arg.as_str()) {
            skip = true;
            continue;
        }
        if ["-c", "-MD", "-MMD", "-MP"].contains(&arg.as_str())
            || arg.starts_with("-x")
            || arg.starts_with("--driver-mode=")
            || arg.starts_with("-working-directory=")
            || (!arg.starts_with('-') && cwd.join(arg).canonicalize().ok().as_deref() == Some(file))
        {
            continue;
        }
        analysis.push(arg.clone());
    }
    analysis.extend([
        "-x".into(),
        if language == Language::C { "c" } else { "c++" }.into(),
        format!("-working-directory={}", cwd.display()),
    ]);
    response_inputs.sort();
    response_inputs.dedup();
    Ok(Replay {
        language,
        arguments: analysis,
        response_inputs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn driver_and_positional_x_override_determine_effective_language() {
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join("input.c");
        std::fs::write(&file, "int run(void){return 1;}").unwrap();
        for (driver, before, after, expected) in [
            ("clang++", vec![], vec![], Language::Cpp),
            ("clang", vec!["-x", "c++"], vec![], Language::Cpp),
            ("clang++", vec!["-xc"], vec![], Language::C),
            ("clang++", vec!["-x", "c"], vec!["-x", "c++"], Language::C),
            (
                "clang",
                vec!["-x", "c++", "-x", "none"],
                vec![],
                Language::C,
            ),
        ] {
            let mut args = vec![driver.to_owned()];
            args.extend(before.into_iter().map(str::to_owned));
            args.push(file.display().to_string());
            args.extend(after.into_iter().map(str::to_owned));
            let r = replay(&args, t.path(), &file).unwrap();
            assert_eq!(r.language, expected);
            assert!(
                r.arguments
                    .windows(2)
                    .any(|p| p[0] == "-x"
                        && p[1] == if expected == Language::C { "c" } else { "c++" })
            );
        }
    }
    #[test]
    fn nested_response_inputs_and_unknown_drivers_are_explicit() {
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join("input.c");
        std::fs::write(&file, "int x;").unwrap();
        std::fs::write(t.path().join("nested.rsp"), "-x c++").unwrap();
        std::fs::write(t.path().join("flags.rsp"), "@nested.rsp -DVALUE=1").unwrap();
        let r = replay(
            &[
                "clang".into(),
                "@flags.rsp".into(),
                file.display().to_string(),
            ],
            t.path(),
            &file,
        )
        .unwrap();
        assert_eq!(r.language, Language::Cpp);
        assert_eq!(r.response_inputs.len(), 2);
        assert!(r.arguments.contains(&"-DVALUE=1".into()));
        assert!(
            replay(
                &["unknown-wrapper".into(), file.display().to_string()],
                t.path(),
                &file
            )
            .is_err()
        );
        assert!(
            replay(
                &["clang-cl".into(), file.display().to_string()],
                t.path(),
                &file
            )
            .is_err()
        );
        assert!(
            replay(
                &[
                    "clang".into(),
                    "--driver-mode=cl".into(),
                    file.display().to_string()
                ],
                t.path(),
                &file
            )
            .is_err()
        );
        let other = t.path().join("other");
        std::fs::create_dir(&other).unwrap();
        assert!(
            replay(
                &[
                    "clang".into(),
                    format!("-working-directory={}", other.display()),
                    file.display().to_string()
                ],
                t.path(),
                &file
            )
            .is_err()
        );
        std::fs::write(t.path().join("nested.rsp"), "@nested.rsp").unwrap();
        assert!(
            replay(
                &[
                    "clang".into(),
                    "@nested.rsp".into(),
                    file.display().to_string()
                ],
                t.path(),
                &file
            )
            .is_err()
        );
    }
}
