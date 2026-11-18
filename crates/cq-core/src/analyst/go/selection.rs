//! Translate the selected build/test command to go list's build selection.
use crate::{Error, Result};
pub(super) fn list_args(args: &[String]) -> Result<Vec<String>> {
    let command = args.first().map(String::as_str).unwrap_or("");
    if !matches!(command, "build" | "test") {
        return Err(Error::new(
            "UnsupportedGoCommand",
            "native analysis requires go build or go test",
        ));
    }
    let mut result = vec![
        "list".into(),
        "-deps".into(),
        "-export".into(),
        "-compiled".into(),
        "-json".into(),
    ];
    if command == "test" {
        result.push("-test".into());
    }
    let mut iter = args[1..].iter();
    while let Some(arg) = iter.next() {
        if arg == "-args" {
            break;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map(|(f, _)| (f, true))
            .unwrap_or((arg.as_str(), false));
        if flag == "-n" {
            return Err(Error::new(
                "UnsupportedGoCommand",
                "dry-run cannot provide an observed compiler build",
            ));
        }
        if matches!(flag, "-a" | "-v" | "-x" | "-work") {
            continue;
        }
        let test_flag = flag
            .strip_prefix("-test.")
            .unwrap_or(flag.trim_start_matches('-'));
        let boolean = matches!(
            test_flag,
            "c" | "json" | "short" | "failfast" | "fullpath" | "benchmem" | "paniconexit0" | "v"
        );
        let valued = matches!(
            test_flag,
            "o" | "exec"
                | "vet"
                | "run"
                | "skip"
                | "list"
                | "bench"
                | "benchtime"
                | "count"
                | "cpu"
                | "parallel"
                | "timeout"
                | "shuffle"
                | "fuzz"
                | "fuzztime"
                | "fuzzminimizetime"
                | "coverprofile"
                | "memprofile"
                | "blockprofile"
                | "cpuprofile"
                | "mutexprofile"
                | "trace"
                | "outputdir"
                | "testlogfile"
                | "gocoverdir"
                | "memprofilerate"
                | "blockprofilerate"
                | "mutexprofilefraction"
        );
        if command == "test" && (boolean || valued) {
            if valued && !inline && iter.next().is_none() {
                return Err(Error::new(
                    "InvalidGoArguments",
                    format!("missing value for {arg}"),
                ));
            }
            continue;
        }
        if flag == "-o" {
            if !inline && iter.next().is_none() {
                return Err(Error::new("InvalidGoArguments", "missing output path"));
            }
            continue;
        }
        result.push(arg.clone());
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).into()).collect()
    }
    #[test]
    fn test_selection_preserves_build_flags_and_package_patterns() {
        let got = list_args(&args(&[
            "test",
            "-a",
            "-vet=off",
            "-run",
            "^$",
            "-count=1",
            "-tags",
            "custom",
            "./api/...",
            "./impl/...",
            "-args",
            "-custom",
        ]))
        .unwrap();
        assert_eq!(
            got,
            args(&[
                "list",
                "-deps",
                "-export",
                "-compiled",
                "-json",
                "-test",
                "-tags",
                "custom",
                "./api/...",
                "./impl/..."
            ])
        );
    }
    #[test]
    fn malformed_and_dry_run_are_rejected() {
        for v in [
            &["run", "x.go"][..],
            &["test", "-run"],
            &["build", "-n"],
            &["test", "-o"],
        ] {
            assert!(list_args(&args(v)).is_err());
        }
    }
    #[test]
    fn build_output_is_not_a_package() {
        assert_eq!(
            list_args(&args(&["build", "-o", "binary", "./..."]))
                .unwrap()
                .last()
                .unwrap(),
            "./..."
        );
    }
}
