use crate::LocalStore;
pub use cq_core::analyst::rust::project::RustProjectPlan as RustProjectView;
use cq_core::{Result, model::NativeCache, ports::ContentReader};
impl LocalStore {
    pub fn materialize_rust(
        &self,
        report: &cq_core::model::CaptureReport,
    ) -> Result<RustProjectView> {
        let root = self.root.join("views").join(&report.run.id).join("hir-v3");
        let plan = cq_core::analyst::rust::project::plan(report, root, &self.content)?;
        std::fs::create_dir_all(&plan.root)?;
        for input in &plan.mapping {
            let target = std::path::Path::new(&input.path);
            std::fs::create_dir_all(target.parent().unwrap())?;
            super::atomic::immutable_write(target, &self.content.read_blob(&input.file.blob)?)?;
        }
        super::atomic::immutable_write(&plan.root.join("rust-project.json"), &plan.manifest)?;
        super::atomic::immutable_write(
            &plan.root.join("input-map.json"),
            &serde_json::to_vec(&plan.mapping)?,
        )?;
        Ok(plan)
    }
}
impl cq_core::ports::AnalysisWorkspace for LocalStore {
    fn semantic_records(&self) -> Option<&dyn cq_core::semantic::records::Store> {
        self.raw
            .as_deref()
            .map(|r| r as &dyn cq_core::semantic::records::Store)
    }
    fn load_native_cache(&self, key: &str) -> Result<Option<NativeCache>> {
        self.read_native_cache(key)
    }
    fn save_native_cache(&self, cache: &NativeCache) -> Result<()> {
        self.write_native_cache(cache)
    }
    fn content(&self) -> &dyn cq_core::ports::ContentRepository {
        &self.content
    }
    fn run_directory(&self, run: &str) -> Result<std::path::PathBuf> {
        self.run_path(run)
    }
    fn create_capture(
        &self,
        run: &str,
    ) -> Result<std::sync::Arc<dyn cq_core::ports::CaptureJournal>> {
        Ok(std::sync::Arc::new(crate::journal::LocalCapture::open(
            self.run_path(run)?,
            run,
            cq_core::model::CaptureBudget::default(),
        )?))
    }
    fn materialize_input(
        &self,
        file: &cq_core::model::FileRevision,
        directory: &std::path::Path,
    ) -> Result<std::path::PathBuf> {
        let target = directory
            .join("inputs")
            .join(&file.id)
            .join(file.logical_path.path().file_name().unwrap_or_default());
        std::fs::create_dir_all(target.parent().unwrap())?;
        crate::atomic::immutable_write(&target, &self.content.read_blob(&file.blob)?)?;
        Ok(target)
    }
}

impl cq_core::analyst::rust::semantic::RustWorkspace for LocalStore {
    fn directory(&self) -> &std::path::Path {
        &self.root
    }
    fn prepare_rust(
        &self,
        report: &cq_core::model::CaptureReport,
        control: &cq_core::control::CaptureControl,
    ) -> Result<RustProjectView> {
        self.materialize_rust_session(report, control)
    }
}

struct MirrorLease(std::fs::File);
impl cq_core::ports::AnalysisLease for MirrorLease {}
impl Drop for MirrorLease {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
impl LocalStore {
    pub fn materialize_rust_session(
        &self,
        report: &cq_core::model::CaptureReport,
        control: &cq_core::control::CaptureControl,
    ) -> Result<RustProjectView> {
        use cq_core::{Error, model::*};
        use std::{io::Write, os::fd::AsRawFd};
        let directory = self
            .root
            .join("mirrors")
            .join(digest(&report.plan.project.0));
        std::fs::create_dir_all(&directory)?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("lease.lock"))?;
        loop {
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK)) {
                return Err(error.into());
            }
            if control.is_cancelled() {
                return Err(Error::new("Cancelled", "waiting for native mirror lease"));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let lease = MirrorLease(lock);
        let root = directory.join("hir-v4");
        let mut plan = cq_core::analyst::rust::project::plan(report, root, &self.content)?;
        std::fs::create_dir_all(&plan.root)?;
        let expected: std::collections::BTreeSet<_> =
            plan.mapping.iter().map(|m| m.path.as_str()).collect();
        let previous = std::fs::read(plan.root.join("input-map.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<MaterializedInput>>(&b).ok());
        if let Some(previous) = previous {
            for old in previous {
                let path = std::path::Path::new(&old.path);
                if path.starts_with(&plan.root)
                    && !path
                        .components()
                        .any(|p| p == std::path::Component::ParentDir)
                    && !expected.contains(old.path.as_str())
                    && path.is_file()
                {
                    std::fs::remove_file(path)?;
                }
            }
        } else if plan.root.join("units").exists() {
            std::fs::remove_dir_all(plan.root.join("units"))?;
        }
        for input in &plan.mapping {
            let target = std::path::Path::new(&input.path);
            std::fs::create_dir_all(target.parent().unwrap())?;
            let bytes = self.content.read_blob(&input.file.blob)?;
            if std::fs::read(target).ok().as_ref() != Some(&bytes) {
                let mut temp = tempfile::NamedTempFile::new_in(target.parent().unwrap())?;
                temp.write_all(&bytes)?;
                temp.as_file().sync_all()?;
                temp.persist(target).map_err(|e| e.error)?;
            }
        }
        let write = |path: &std::path::Path, bytes: &[u8]| -> Result<()> {
            let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
            temp.write_all(bytes)?;
            temp.as_file().sync_all()?;
            temp.persist(path).map_err(|e| e.error)?;
            Ok(())
        };
        write(&plan.root.join("rust-project.json"), &plan.manifest)?;
        write(
            &plan.root.join("input-map.json"),
            &serde_json::to_vec(&plan.mapping)?,
        )?;
        let evidence = self.run_path(&report.run.id)?;
        super::atomic::immutable_write(
            &evidence.join("semantic-input-map.json"),
            &serde_json::to_vec(&plan.mapping)?,
        )?;
        super::atomic::immutable_write(&evidence.join("semantic-project.json"), &plan.manifest)?;
        plan.lease = Some(Box::new(lease));
        Ok(plan)
    }
}
