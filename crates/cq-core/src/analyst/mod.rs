pub mod native;
pub mod rust;
use crate::{Result, control::CaptureControl, model::*, ports::*};
pub trait BuildObserver {
    type Request;
    fn capabilities(&self) -> CaptureCapabilities;
    fn plan(&self, request: Self::Request) -> Result<CapturePlan>;
    fn observe(
        &self,
        plan: CapturePlan,
        journal: std::sync::Arc<dyn CaptureJournal>,
        content: &dyn ContentRepository,
        control: &CaptureControl,
    ) -> Result<CaptureReport>;
}

pub mod languages;

pub mod cpp;
pub mod go;
