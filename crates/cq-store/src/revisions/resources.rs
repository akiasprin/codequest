use cq_core::{Error, Result};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
fn reserved() -> &'static Mutex<BTreeMap<PathBuf, u64>> {
    static R: OnceLock<Mutex<BTreeMap<PathBuf, u64>>> = OnceLock::new();
    R.get_or_init(Default::default)
}
pub fn bytes(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    for e in std::fs::read_dir(path)? {
        let e = e?;
        let ty = e.file_type()?;
        let size = if ty.is_dir() {
            bytes(&e.path())?
        } else if ty.is_file() {
            e.metadata()?.len()
        } else {
            0
        };
        total = total
            .checked_add(size)
            .ok_or_else(|| Error::new("MaintenanceBudget", "size overflow"))?;
    }
    Ok(total)
}
pub struct Reservation {
    root: PathBuf,
    pub bytes: u64,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut r = reserved().lock().unwrap();
        if let Some(n) = r.get_mut(&self.root) {
            *n = n.saturating_sub(self.bytes);
        }
    }
}
pub fn reserve(project_root: &Path, current_epoch: &str) -> Result<Reservation> {
    let limit = std::env::var("CQ_STORE_MAINTENANCE_DISK_BYTES")
        .ok()
        .map(|s| s.parse::<u64>())
        .transpose()
        .map_err(|_| Error::new("MaintenanceBudget", "invalid disk limit"))?
        .unwrap_or(32 * 1024 * 1024 * 1024);
    reserve_limited(project_root, current_epoch, limit)
}
fn reserve_limited(project_root: &Path, current_epoch: &str, limit: u64) -> Result<Reservation> {
    let format = project_root
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| Error::new("MaintenanceBudget", "store root absent"))?;
    let root = format.join("projects");
    let source = bytes(&project_root.join("indexes").join(current_epoch))?;
    let needed = source
        .saturating_mul(2)
        .saturating_add(64 * 1024 * 1024)
        .clamp(256 * 1024 * 1024, 16 * 1024 * 1024 * 1024);
    let mut reservations = reserved().lock().unwrap();
    let pending = reservations.get(&root).copied().unwrap_or(0);
    let existing = bytes(&root)?;
    if existing.saturating_add(pending).saturating_add(needed) > limit {
        return Err(Error::new(
            "MaintenanceBudget",
            format!(
                "native/store metadata {existing} + pending {pending} + reserved {needed} exceeds {limit}; release old epochs or increase the explicit maintenance budget"
            ),
        ));
    }
    reservations.insert(root.clone(), pending + needed);
    Ok(Reservation {
        root,
        bytes: needed,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reservations_reject_before_creating_an_epoch_and_release_on_drop() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("format-2/projects/one");
        assert_eq!(
            reserve_limited(&p, "first", 1).err().unwrap().code(),
            "MaintenanceBudget"
        );
        assert!(!p.exists());
        let one = reserve_limited(&p, "first", 512 * 1024 * 1024).unwrap();
        let two = reserve_limited(&p, "second", 512 * 1024 * 1024).unwrap();
        assert!(reserve_limited(&p, "third", 512 * 1024 * 1024).is_err());
        drop(one);
        let three = reserve_limited(&p, "third", 512 * 1024 * 1024).unwrap();
        drop((two, three));
        assert_eq!(
            reserved()
                .lock()
                .unwrap()
                .get(&t.path().join("format-2/projects")),
            Some(&0)
        );
    }
}
