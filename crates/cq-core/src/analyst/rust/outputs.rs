use super::args;
use crate::model::*;
use std::os::unix::fs::MetadataExt;

/// Cargo can hard-link a binary from rustc's out-dir into the profile root and
/// report only that published path. Record the exact filesystem identity rather
/// than selecting an invocation by target name or arrival time.
pub(super) fn collect_aliases(invocations: &[CompilerInvocation], artifacts: &mut [BuildArtifact]) {
    for inv in invocations {
        let a = args::inspect(inv);
        if a.probe {
            continue;
        }
        let (Some(dir), Some(name), Some(root)) = (&a.out_dir, &a.name, &a.root) else {
            continue;
        };
        let output = NativeValue::new(dir.join(format!("{name}{}", a.extra)));
        let Ok(compiled) = std::fs::metadata(output.path()) else {
            continue;
        };
        if !compiled.is_file() {
            continue;
        }
        for artifact in artifacts
            .iter_mut()
            .filter(|x| &x.src_path == root && x.profile.test == a.test)
        {
            for path in &artifact.paths {
                if args::output_matches(&a, path) {
                    continue;
                }
                let Ok(published) = std::fs::metadata(path.path()) else {
                    continue;
                };
                if published.is_file()
                    && compiled.dev() == published.dev()
                    && compiled.ino() == published.ino()
                {
                    artifact.output_aliases.push(OutputAlias {
                        invocation_id: inv.id.clone(),
                        reported_path: path.clone(),
                        compiler_output: output.clone(),
                        device: compiled.dev(),
                        inode: compiled.ino(),
                        observed_ms: cq_capture_protocol::now_ms(),
                    });
                }
            }
        }
    }
}
