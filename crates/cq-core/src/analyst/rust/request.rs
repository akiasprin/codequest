use crate::model::CaptureBudget;
use std::path::PathBuf;
#[derive(Clone, Debug)]
pub struct RustBuildRequest {
    pub project: PathBuf,
    pub output: PathBuf,
    pub cargo: PathBuf,
    pub recorder: PathBuf,
    pub args: Vec<String>,
    pub mode: String,
    pub reuse_target: Option<PathBuf>,
    pub extra_env_keys: Vec<String>,
    pub budget: CaptureBudget,
}
