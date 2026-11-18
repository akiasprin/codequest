//! Regression contracts for every finding from the independent R0 review.
use crate::{
    host::Host,
    model::*,
    projection::{Projection, quote},
};
use serde_json::{Value, json};
use std::{fs, path::Path, sync::Arc};

fn initial(root: &Path) -> Result<(Arc<Host>, Candidate)> {
    let h = Host::create(root)?;
    let r = h.prepare(
        "initial",
        None,
        &fixture("A", "prod"),
        &fixture_sources("A"),
    )?;
    h.publish(&r, None)?;
    Ok((h, r))
}
fn next(h: &Host, job: &str, target: &str) -> Result<Candidate> {
    h.prepare(
        job,
        h.head()?,
        &fixture(target, "prod"),
        &fixture_sources(target),
    )
}
fn target(h: &Arc<Host>, revision: Option<&str>, expected: &str) -> Result<()> {
    let v = h.open_view(revision, "prod", 1000)?;
    let read = h.start_read(&v)?;
    require(
        read.edges("prod::entry", "", 10)?[0].1 == format!("prod::{expected}"),
        "wrong graph target",
    )?;
    drop(read);
    h.release(&v)
}

pub fn sealed_dependencies(root: &Path) -> Result<Value> {
    let variants = [
        "missing_new_fact",
        "tampered_new_fact",
        "missing_new_source",
        "tampered_new_source",
        "missing_reused_fact",
        "tampered_reused_source",
        "wrong_length",
        "wrong_seal",
    ];
    let mut evidence = vec![];
    for variant in variants {
        let dir = root.join(variant);
        let (h, r1) = initial(&dir)?;
        let mut r2 = next(&h, "second", "B")?;
        let old = &r2.manifest.shards["prod/targets"];
        let new = &r2.manifest.shards["prod/entry"];
        match variant {
            "missing_new_fact" => fs::remove_file(dir.join("facts").join(&new.hash))?,
            "tampered_new_fact" => fs::write(dir.join("facts").join(&new.hash), b"tampered")?,
            "missing_new_source" => fs::remove_file(dir.join("evidence").join(&new.source))?,
            "tampered_new_source" => {
                fs::write(dir.join("evidence").join(&new.source), b"tampered")?
            }
            "missing_reused_fact" => fs::remove_file(dir.join("facts").join(&old.hash))?,
            "tampered_reused_source" => {
                fs::write(dir.join("evidence").join(&old.source), b"tampered")?
            }
            "wrong_length" => {
                r2.manifest
                    .shards
                    .get_mut("prod/entry")
                    .ok_or("entry")?
                    .source_bytes += 1;
                r2.revision = r2.manifest.revision()?;
                r2.seal = r2.seal()?;
            }
            "wrong_seal" => r2.seal = digest(b"forged"),
            _ => unreachable!(),
        }
        let failure = h
            .publish(&r2, None)
            .expect_err("incomplete candidate was published");
        require(
            failure.outcome == CommitState::NotCommitted
                && h.head()?.as_deref() == Some(&r1.revision),
            "bad candidate changed publication decision",
        )?;
        require(
            h.native_pending_count()? == 0,
            "bad candidate reached index commit",
        )?;
        evidence.push(json!({"variant":variant,"outcome":failure.outcome,"error":failure.message,"head_unchanged":true}));
    }
    Ok(json!({"dependency_cases":evidence,"readiness_requires_fact_and_source_seal":true}))
}

pub fn immutable_view(root: &Path) -> Result<Value> {
    let (h, r1) = initial(root)?;
    let old = h.open_view(None, "prod", 1000)?;
    let read = h.start_read(&old)?;
    let r2 = next(&h, "second", "B")?;
    h.publish(&r2, None)?;
    let mut forged = old.clone();
    forged.sequence = r2.manifest.sequence;
    require(
        h.start_read(&forged).is_err(),
        "forged view sequence admitted",
    )?;
    forged = old.clone();
    forged.context = "test".into();
    require(h.start_read(&forged).is_err(), "forged context admitted")?;
    forged = old.clone();
    forged.epoch = "e2".into();
    require(h.start_read(&forged).is_err(), "forged epoch admitted")?;
    require(
        read.view().revision == r1.revision
            && read.edges("prod::entry", "", 10)?[0].1 == "prod::A"
            && read.source("prod/entry")? == fixture_sources("A")[1],
        "read guard mixed versions",
    )?;
    Ok(
        json!({"forged_sequence_context_epoch_rejected":true,"read_view_is_read_only":true,"old_graph_and_source_agree":true}),
    )
}

pub fn adjacency_integrity(root: &Path) -> Result<Value> {
    let variants = [
        "missing_head",
        "tampered_head",
        "missing_first_page",
        "tampered_page",
        "valid_terminal",
        "invalid_cursor",
        "valid_empty_subject",
    ];
    let mut evidence = vec![];
    for variant in variants {
        let dir = root.join(variant);
        let (h, r1) = initial(&dir)?;
        let view = h.open_view(None, "prod", 1000)?;
        let read = h.start_read(&view)?;
        let hash = &r1.manifest.shards["prod/entry"].hash;
        let p = h.current_projection()?;
        let c = p.connection()?;
        let page = Projection::adjacency_key(hash, "prod::entry", 0);
        let head = Projection::adjacency_head_key(hash, "prod::entry");
        match variant {
            "missing_head" => p.exec(
                &c,
                &format!("MATCH (h:AdjacencyHead {{id:{}}}) DELETE h", quote(&head)),
            )?,
            "tampered_head" => p.exec(
                &c,
                &format!(
                    "MATCH (h:AdjacencyHead {{id:{}}}) SET h.directory='{{}}'",
                    quote(&head)
                ),
            )?,
            "missing_first_page" => p.exec(
                &c,
                &format!("MATCH (p:AdjacencyPage {{id:{}}}) DELETE p", quote(&page)),
            )?,
            "tampered_page" => p.exec(
                &c,
                &format!(
                    "MATCH (p:AdjacencyPage {{id:{}}}) SET p.items='[]'",
                    quote(&page)
                ),
            )?,
            _ => {}
        }
        if variant == "valid_terminal" {
            require(
                read.adjacency_page(hash, "prod::entry", 1)?.is_empty(),
                "normal terminal rejected",
            )?;
        } else if variant == "valid_empty_subject" {
            let common = &r1.manifest.shards["prod/targets"].hash;
            require(
                read.adjacency_page(common, "prod::A", 0)?.is_empty(),
                "authenticated zero-degree rejected",
            )?;
        } else {
            let ordinal = if variant == "invalid_cursor" { 2 } else { 0 };
            let error = read
                .adjacency_page(hash, "prod::entry", ordinal)
                .expect_err("corrupt page mistaken for empty")
                .to_string();
            require(
                error.contains("IndexIncomplete") || error.contains("InvalidAdjacencyCursor"),
                &error,
            )?;
        }
        require(
            read.edges("prod::entry", "", 10)?.len() == 1,
            "canonical relation control failed",
        )?;
        evidence.push(json!({"variant":variant,"contract_checked":true}));
    }
    Ok(
        json!({"cases":evidence,"missing_is_not_empty":true,"normal_end_and_zero_degree_supported":true}),
    )
}

pub fn relation_identity(root: &Path) -> Result<Value> {
    let h = Host::create(root)?;
    let mut payloads = fixture("A", "prod");
    payloads[1].edges.push(Edge {
        from: "prod::entry".into(),
        to: "prod::B".into(),
        occurrence: "call@1".into(),
    });
    let r1 = h.prepare("parallel-targets", None, &payloads, &fixture_sources("A"))?;
    h.publish(&r1, None)?;
    let view = h.open_view(None, "prod", 1000)?;
    let read = h.start_read(&view)?;
    let full = read.edges("prod::entry", "", 10)?;
    let first = read.edges("prod::entry", "", 1)?;
    let second = read.edges("prod::entry", &first[0].0, 1)?;
    require(
        full.len() == 2 && first.len() == 1 && second.len() == 1 && first[0].0 != second[0].0,
        "equal occurrence lost a distinct relation",
    )?;
    require(
        read.edges("prod::entry", &second[0].0, 1)?.is_empty(),
        "bad terminal",
    )?;
    let page = read.adjacency_page(&r1.manifest.shards["prod/entry"].hash, "prod::entry", 0)?;
    require(
        page == full,
        "paged native and derived edge identities differ",
    )?;
    let duplicate = payloads[1].edges[0].clone();
    payloads[1].edges.push(duplicate);
    let error = h
        .prepare("duplicate", h.head()?, &payloads, &fixture_sources("A"))
        .expect_err("duplicate relation accepted")
        .to_string();
    require(error.contains("DuplicateRelation"), &error)?;
    Ok(
        json!({"same_occurrence_distinct_relations":2,"keyset_page_size":1,"no_loss_or_duplicates":true,"exact_duplicate_rejected":true}),
    )
}

pub fn retained_rebuild(root: &Path) -> Result<Value> {
    let mut evidence = vec![];
    for gap in [false, true] {
        let dir = root.join(if gap { "retained-gap" } else { "only-latest" });
        let (h, r1) = initial(&dir)?;
        let old = if gap {
            Some(h.open_view(None, "prod", 1000)?)
        } else {
            None
        };
        let r2 = next(&h, "second", "B")?;
        h.publish(&r2, None)?;
        let current = if gap {
            let r3 = next(&h, "third", "A")?;
            h.publish(&r3, None)?;
            r3
        } else {
            r2.clone()
        };
        let retire = if gap { &r2.revision } else { &r1.revision };
        let plan = h.plan_gc(std::slice::from_ref(retire))?;
        require(
            h.sweep(&plan)? == 2,
            "expected obsolete fact and source reclamation",
        )?;
        h.audit_manifest(&current.manifest)?;
        let route = h.build_epoch("e2")?;
        require(
            h.collect_epoch("e2").is_err(),
            "maintenance epoch collected",
        )?;
        h.switch_epoch("e2", &route, None)?;
        target(&h, None, if gap { "A" } else { "B" })?;
        require(
            h.open_view(Some(retire), "prod", 1000).is_err(),
            "reclaimed revision resurrected",
        )?;
        if let Some(old) = old {
            let read = h.start_read(&old)?;
            require(
                read.edges("prod::entry", "", 10)?[0].1 == "prod::A",
                "retained old view changed",
            )?;
        }
        evidence.push(json!({"history_gap":gap,"rebuild_after_gc":true,"canonical_current_root_verified":true}));
    }
    Ok(json!({"cases":evidence,"rebuild_uses_retained_full_roots":true}))
}

pub fn oracle_guards(root: &Path) -> Result<Value> {
    let (h, r1) = initial(root)?;
    let r2 = next(&h, "second", "B")?;
    h.publish(&r2, None)?;
    let plan = h.plan_gc(&[r1.revision])?;
    h.sweep(&plan)?;
    h.audit_manifest(&r2.manifest)?;
    let hash = &r2.manifest.shards["prod/targets"].hash;
    fs::remove_file(root.join("facts").join(hash))?;
    require(
        h.audit_manifest(&r2.manifest).is_err(),
        "strengthened oracle missed a deleted retained object",
    )?;
    Ok(json!({"deliberate_mutation_detected":true,"canonical_fact_and_evidence_audit_used":true}))
}

pub fn commit_fault(root: &Path) -> Result<Value> {
    let mode = std::env::var("CQ_R0_FAULT_MODE")
        .map_err(|_| "fault regression requires the dedicated runner")?;
    let (h, r1) = initial(root)?;
    let old = h.open_view(None, "prod", 1000)?;
    let r2 = next(&h, "second", "B")?;
    fs::write(root.join("INJECT_DIRECTORY_FSYNC"), b"armed")?;
    let failure = h.publish(&r2, None).expect_err("fault was not injected");
    require(
        failure.outcome == CommitState::Unknown,
        "ambiguous outcome was reported as definite failure",
    )?;
    require(
        h.open_view(None, "prod", 1000).is_err(),
        "uncertain durability admitted a new view",
    )?;
    let read = h.start_read(&old)?;
    require(
        read.edges("prod::entry", "", 10)?[0].1 == "prod::A",
        "old read broken during recovery",
    )?;
    drop(read);
    let first = h.resolve_commit(&r2);
    let repeated = mode == "directory-repeat";
    if repeated {
        require(
            first.outcome == CommitState::Unknown && h.open_view(None, "prod", 1000).is_err(),
            "persistent I/O fault unblocked admission",
        )?;
    }
    fs::remove_file(root.join("INJECT_DIRECTORY_FSYNC"))?;
    let resolved = h.resolve_commit(&r2);
    let expected = if mode == "head-file" {
        CommitState::NotCommitted
    } else {
        CommitState::Committed
    };
    require(
        resolved.outcome == expected && resolved.error.is_none(),
        "commit resolution is wrong",
    )?;
    require(
        h.head()?.as_deref()
            == Some(if expected == CommitState::Committed {
                &r2.revision
            } else {
                &r1.revision
            }),
        "HEAD disagrees with outcome",
    )?;
    target(
        &h,
        None,
        if expected == CommitState::Committed {
            "B"
        } else {
            "A"
        },
    )?;
    require(
        h.resolve_commit(&r2).outcome == expected,
        "resolution not idempotent",
    )?;
    Ok(
        json!({"fault_mode":mode,"initial_outcome":failure.outcome,"first_resolution":first,"final_resolution":resolved,"admission_blocked_while_unknown":true,"old_view_preserved":true}),
    )
}
