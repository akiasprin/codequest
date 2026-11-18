//! Process-wide residency assertions run in their own integration binary.
use cq_core::ports::facts::*;
use cq_store::{LocalStore, revisions::StoreHost};
#[path = "common/paged_fixture.rs"]
mod paged_fixture;
use paged_fixture::{fixture, write};
#[test]
fn r5_logical_views_do_not_pin_native_databases_and_old_epoch_reopens() {
    let t = tempfile::tempdir().unwrap();
    let store = LocalStore::open(t.path()).unwrap();
    let mut hosts = vec![];
    let mut views = vec![];
    for name in ["residency-a", "residency-b", "residency-c"] {
        let h = StoreHost::open(t.path(), name).unwrap();
        let mut s = fixture(&store, 3, 1);
        s.project = name.into();
        s.seal().unwrap();
        let r = write(&h.objects, &store, &s, None);
        assert_eq!(h.publish(&r, None).outcome, CommitOutcome::Committed);
        views.push(h.open_view(None).unwrap());
        hosts.push(h);
    }
    for _ in 0..2 {
        for v in &views {
            assert_eq!(v.get("entity", "f0").unwrap().unwrap().value["name"], "f0");
            assert_eq!(
                cq_store::ladybug::residency::diagnostics()["ordinary_databases"],
                1
            );
        }
    }
    let old = views[0].epoch().to_owned();
    let new = hosts[0].rebuild().unwrap();
    assert_ne!(old, new);
    assert!(hosts[0].root.join("indexes").join(&old).exists());
    assert_eq!(
        views[0].get("entity", "f0").unwrap().unwrap().value["name"],
        "f0"
    );
    drop(views);
    assert!(hosts[0].collect_epochs().unwrap().contains(&old));
    assert_eq!(
        cq_store::ladybug::residency::diagnostics()["maintenance_databases"],
        0
    );
}
