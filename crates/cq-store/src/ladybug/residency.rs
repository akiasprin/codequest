//! Logical revision leases outlive native connections. One ordinary database
//! and one separately reserved maintenance database may be resident at once.
use super::Projection;
use cq_core::{Error, Result};
use std::{
    ops::Deref,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::{Duration, Instant},
};
pub const BUFFER_BYTES: u64 = 512 * 1024 * 1024;
struct Slot {
    path: PathBuf,
    db: Arc<Projection>,
}
#[derive(Default)]
struct State {
    ordinary: Option<Slot>,
    maintenance: Option<Slot>,
    waiters: usize,
    opens: u64,
    evictions: u64,
}
#[derive(Default)]
struct Pool {
    state: Mutex<State>,
    changed: Condvar,
}
fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(Pool::default)
}
pub struct Lease(Option<Arc<Projection>>);
impl Deref for Lease {
    type Target = Projection;
    fn deref(&self) -> &Projection {
        self.0.as_deref().unwrap()
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        drop(self.0.take());
        let mut s = pool().state.lock().unwrap();
        if s.maintenance
            .as_ref()
            .is_some_and(|v| Arc::strong_count(&v.db) == 1)
        {
            let slot = s.maintenance.take();
            if s.ordinary
                .as_ref()
                .is_none_or(|v| Arc::strong_count(&v.db) == 1)
            {
                if s.ordinary.take().is_some() {
                    s.evictions += 1;
                }
                s.ordinary = slot;
            } else {
                drop(slot);
                s.evictions += 1;
            }
        }
        drop(s);
        pool().changed.notify_all();
    }
}
pub fn acquire(path: &Path, create: bool, maintenance: bool, timeout: Duration) -> Result<Lease> {
    acquire_limited(path, create, maintenance, timeout, 16 * 1024 * 1024 * 1024)
}
pub fn acquire_limited(
    path: &Path,
    create: bool,
    maintenance: bool,
    timeout: Duration,
    disk_limit: u64,
) -> Result<Lease> {
    let pool = pool();
    let started = Instant::now();
    let path = path.to_owned();
    let mut state = pool.state.lock().unwrap();
    loop {
        // Promote a completed maintenance epoch once the old ordinary request
        // releases its connection. Logical old views do not block this step.
        if !maintenance
            && state.maintenance.as_ref().is_some_and(|s| s.path == path)
            && state
                .ordinary
                .as_ref()
                .is_none_or(|s| Arc::strong_count(&s.db) == 1)
        {
            if state.ordinary.take().is_some() {
                state.evictions += 1;
            }
            state.ordinary = state.maintenance.take();
        }
        for slot in [&state.ordinary, &state.maintenance].into_iter().flatten() {
            if slot.path == path {
                return Ok(Lease(Some(slot.db.clone())));
            }
        }
        let slot = if maintenance {
            &mut state.maintenance
        } else {
            &mut state.ordinary
        };
        if slot.as_ref().is_none_or(|s| Arc::strong_count(&s.db) == 1) {
            if slot.take().is_some() {
                state.evictions += 1;
            }
            let db = Arc::new(Projection::open_limited(
                path.as_path(),
                create,
                disk_limit,
            )?);
            let slot = Slot {
                path: path.clone(),
                db: db.clone(),
            };
            if maintenance {
                state.maintenance = Some(slot)
            } else {
                state.ordinary = Some(slot)
            };
            state.opens += 1;
            return Ok(Lease(Some(db)));
        }
        let Some(left) = timeout
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
        else {
            return Err(Error::new(
                "StoreBusy",
                "database residency budget is active; request deadline expired",
            ));
        };
        state.waiters += 1;
        let (next, _) = pool.changed.wait_timeout(state, left).unwrap();
        state = next;
        state.waiters -= 1;
    }
}
pub fn evict(path: &Path) -> bool {
    let mut state = pool().state.lock().unwrap();
    for slot in [&state.ordinary, &state.maintenance].into_iter().flatten() {
        if slot.path == path && Arc::strong_count(&slot.db) > 1 {
            return false;
        }
    }
    if state.ordinary.as_ref().is_some_and(|s| s.path == path) {
        state.ordinary = None;
        state.evictions += 1;
    }
    if state.maintenance.as_ref().is_some_and(|s| s.path == path) {
        state.maintenance = None;
        state.evictions += 1;
    }
    pool().changed.notify_all();
    true
}
pub fn diagnostics() -> serde_json::Value {
    let s = pool().state.lock().unwrap();
    serde_json::json!({"ordinary_databases":usize::from(s.ordinary.is_some()),"maintenance_databases":usize::from(s.maintenance.is_some()),"ordinary_limit":1,"maintenance_limit":1,"buffer_pool_bytes_per_database":BUFFER_BYTES,"waiters":s.waiters,"opens":s.opens,"evictions":s.evictions})
}

pub fn evict_below(root: &Path) {
    let mut s = pool().state.lock().unwrap();
    if s.ordinary
        .as_ref()
        .is_some_and(|v| v.path.starts_with(root) && Arc::strong_count(&v.db) == 1)
    {
        s.ordinary = None;
        s.evictions += 1;
    }
    if s.maintenance
        .as_ref()
        .is_some_and(|v| v.path.starts_with(root) && Arc::strong_count(&v.db) == 1)
    {
        s.maintenance = None;
        s.evictions += 1;
    }
    pool().changed.notify_all();
}
