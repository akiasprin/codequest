//! Execution artifacts are GC roots until explicitly discarded. An unfinished
//! writer has not produced a traversable tree yet, so collection must wait.
use super::*;
use crate::facts::writer::{Revision, Roots, WriteStats};
impl Execution {
    pub(crate) fn retained_roots(root: &Path) -> Result<Vec<Revision>> {
        let mut revisions = vec![];
        let runs = root.join("executions");
        if !runs.exists() {
            return Ok(revisions);
        }
        for entry in std::fs::read_dir(&runs)? {
            let path = entry?.path();
            if !path.join("state.json").exists() {
                continue;
            }
            let e = load(&path)?;
            let writing = [Stage::Encode, Stage::Directories].into_iter().any(|s| {
                e.run.current(s).is_some_and(|a| {
                    matches!(
                        a.state,
                        StageState::Running | StageState::Interrupted | StageState::Failed
                    )
                })
            });
            let encoded = e
                .run
                .current(Stage::Encode)
                .is_some_and(|a| a.state == StageState::Completed);
            let complete = e
                .run
                .current(Stage::Directories)
                .is_some_and(|a| a.state == StageState::Completed);
            if writing || encoded && !complete {
                return Err(Error::new(
                    "ExecutionPinsPending",
                    format!(
                        "execution {} has unfinished encoding/directory roots; resume it before object collection",
                        e.run.id
                    ),
                ));
            }
            if let Some(attempts) = e.run.stages.get(&Stage::Directories) {
                for a in attempts {
                    let Some(point) = a
                        .checkpoint
                        .as_ref()
                        .filter(|p| p.total == Some(p.completed))
                    else {
                        continue;
                    };
                    point.verify()?;
                    let artifact = &point.artifacts[0];
                    let output_path = path.join(&artifact.path);
                    let bytes = std::fs::read(&output_path)?;
                    if digest(&bytes) != artifact.sha256 || bytes.len() as u64 != artifact.bytes {
                        return Err(Error::new("DigestMismatch", "execution GC root"));
                    }
                    let output: Output = serde_json::from_slice(&bytes)?;
                    let name = output.cursor["roots"].as_str().ok_or_else(|| {
                        Error::new("InvalidStageArtifact", "execution directory roots")
                    })?;
                    let image = output
                        .files
                        .iter()
                        .find(|i| i.path == Path::new(name))
                        .ok_or_else(|| {
                            Error::new("InvalidStageArtifact", "unsealed execution root file")
                        })?;
                    let actual = image_file(&image.path)?;
                    if actual.sha256 != image.sha256 || actual.bytes != image.bytes {
                        return Err(Error::new("DigestMismatch", "execution roots"));
                    }
                    let (roots, _): (Roots, WriteStats) =
                        serde_json::from_reader(File::open(name)?)?;
                    let mut r = Revision {
                        format: 2,
                        project: output.header.project,
                        revision: output.header.revision,
                        parent: None,
                        sequence: 1,
                        roots,
                        changes: WriteStats::default(),
                        metadata: json!({"execution_pin":e.run.id}),
                        seal: String::new(),
                    };
                    r.seal()?;
                    revisions.push(r);
                }
            }
        }
        Ok(revisions)
    }
    pub fn discard(root: &Path, id: &str) -> Result<Value> {
        let path = directory(root, id)?;
        let _owner = Self::lock(&path)?;
        let e = load(&path)?;
        if e.run.id != id {
            return Err(Error::new("CorruptExecution", "run identity"));
        }
        let unfinished_backend = e
            .run
            .plan
            .iter()
            .filter(|s| **s >= Stage::AuditInventory && **s <= Stage::Publish)
            .any(|s| {
                e.run.current(*s).is_some_and(|a| {
                    matches!(
                        a.state,
                        StageState::Running | StageState::Interrupted | StageState::Failed
                    )
                })
            });
        if unfinished_backend {
            return Err(Error::new(
                "ExecutionBusy",
                "finish/reconnect the StoreHost stage before discarding this execution",
            ));
        }
        let _gate = crate::revisions::retention::ObjectGate::lock(&root.join("format-2"))?;
        let canonical = path.canonicalize()?;
        if canonical.parent() != Some(runs_directory(root)?.as_path()) {
            return Err(Error::new("InvalidExecutionId", "execution symlink"));
        }
        std::fs::remove_dir_all(&path)?;
        File::open(root.join("executions"))?.sync_all()?;
        Ok(
            json!({"state":"execution_discarded","execution":id,"published_revisions_unchanged":true}),
        )
    }
}
fn runs_directory(root: &Path) -> Result<PathBuf> {
    Ok(root.join("executions").canonicalize()?)
}
fn image_file(path: &Path) -> Result<raw::Image> {
    super::image(path)
}
