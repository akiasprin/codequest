use crate::{
    host::{Host, View},
    model::*,
    projection::{Projection, config, string},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Barrier, mpsc},
    time::{Duration, Instant},
};

fn prepare(host: &Host, job: &str, target: &str) -> Result<Candidate> {
    host.prepare(
        job,
        host.head()?,
        &fixture(target, "prod"),
        &fixture_sources(target),
    )
}
fn initial(root: &Path) -> Result<(Arc<Host>, Candidate)> {
    let h = Host::create(root)?;
    let c = prepare(&h, "initial", "A")?;
    h.publish(&c, None)?;
    Ok((h, c))
}
fn target(host: &Arc<Host>, view: &View, expected: &str) -> Result<()> {
    let read = host.start_read(view)?;
    let edges = read.edges(&format!("{}::entry", view.context), "", 100)?;
    require(
        edges.len() == 1 && edges[0].1 == format!("{}::{expected}", view.context),
        format!("wrong target: {edges:?}, expected {expected}"),
    )
}
fn latest(host: &Arc<Host>, expected: &str) -> Result<()> {
    let view = host.open_view(None, "prod", 1000)?;
    target(host, &view, expected)?;
    host.release(&view)
}

pub fn multiversion(root: &Path) -> Result<Value> {
    let h = Host::create(root)?;
    let mut inputs = fixture("A", "prod");
    inputs.extend(fixture("B", "test"));
    let mut sources = fixture_sources("A");
    sources.extend(fixture_sources("B"));
    let r1 = h.prepare("r1", None, &inputs, &sources)?;
    let first = h.publish(&r1, None)?;
    let v1 = h.open_view(None, "prod", 1000)?;
    let t1 = h.open_view(None, "test", 1000)?;
    target(&h, &v1, "A")?;
    target(&h, &t1, "B")?;
    inputs[1] = fixture("B", "prod")[1].clone();
    let r2 = h.prepare("r2", h.head()?, &inputs, &fixture_sources("B"))?;
    let delta = h.publish(&r2, None)?;
    require(
        delta.payloads == 1
            && delta.nodes == 1
            && delta.edges == 1
            && delta.uses_started == 1
            && delta.uses_closed == 1,
        "delta imported unchanged payloads",
    )?;
    let v2 = h.open_view(None, "prod", 1000)?;
    target(&h, &v1, "A")?;
    target(&h, &v2, "B")?;
    target(&h, &t1, "B")?;
    {
        let a = h.start_read(&v1)?;
        let b = h.start_read(&v2)?;
        require(
            a.entities("author document for B")?.is_empty(),
            "future document leaked into old revision",
        )?;
        require(
            b.entities("author document for B")?.len() == 1,
            "new document missing",
        )?;
        require(
            a.visible_entity_count()? == 4 && b.visible_entity_count()? == 4,
            "mixed revision counts",
        )?;
        require(
            a.sequence_from_receipt()? == v1.sequence && b.sequence_from_receipt()? == v2.sequence,
            "receipt version mismatch",
        )?;
        require(
            a.source("prod/entry")? == fixture_sources("A")[1],
            "old evidence changed",
        )?;
        require(
            b.source("prod/entry")? == fixture_sources("B")[1],
            "new evidence wrong",
        )?;
        let old_hash = &r1.manifest.shards["prod/entry"].hash;
        require(
            a.adjacency_page(old_hash, "prod::entry", 0)?[0].1 == "prod::A",
            "adjacency page lost old version",
        )?;
        require(
            b.adjacency_page(old_hash, "prod::entry", 0).is_err(),
            "old payload accepted in new adjacency view",
        )?;
    }
    inputs[1] = fixture("A", "prod")[1].clone();
    let r3 = h.prepare("r3", h.head()?, &inputs, &fixture_sources("A"))?;
    let aba = h.publish(&r3, None)?;
    require(
        aba.payloads == 0 && aba.nodes == 0 && aba.edges == 0,
        "A->B->A re-imported existing payload",
    )?;
    require(
        h.native_payload_count()? == 5,
        "physical payload deduplication failed",
    )?;
    require(
        h.interval_count(&r1.manifest.shards["prod/entry"].hash)? == 2,
        "A did not get two visibility intervals",
    )?;
    let v3 = h.open_view(None, "prod", 1000)?;
    target(&h, &v1, "A")?;
    target(&h, &v2, "B")?;
    target(&h, &v3, "A")?;
    let r4 = h.prepare(
        "same-facts-new-observation",
        h.head()?,
        &inputs,
        &fixture_sources("A"),
    )?;
    let noop = h.publish(&r4, None)?;
    require(
        noop.payloads == 0 && noop.uses_started == 0 && noop.uses_closed == 0,
        "same facts produced graph delta",
    )?;
    require(
        h.publish(&r4, None)?.payloads == 0 && h.head()?.as_deref() == Some(&r4.revision),
        "retry not idempotent",
    )?;
    let mut mismatched_retry = r4.clone();
    mismatched_retry.manifest.sequence += 10;
    let mismatch = h
        .publish(&mismatched_retry, None)
        .expect_err("mismatched retry accepted")
        .to_string();
    require(
        mismatch.contains("CandidateMismatch") && h.head()?.as_deref() == Some(&r4.revision),
        "retry mismatch did not fail closed",
    )?;
    require(
        h.open_view(Some("not-published"), "prod", 1000).is_err(),
        "uncommitted revision admitted",
    )?;
    Ok(
        json!({"initial":first,"change":delta,"aba":aba,"same_facts_new_observation":noop,"physical_payloads":h.native_payload_count()?,"views":[v1,v2,v3],"actual_compilation":false}),
    )
}

pub fn deletion_and_rollback(root: &Path) -> Result<Value> {
    let (h, r1) = initial(root)?;
    let view = h.open_view(None, "prod", 1000)?;
    let mut bad = fixture("A", "prod");
    bad[0].nodes.retain(|n| n.name != "A");
    let invalid = h.prepare("dangling", h.head()?, &bad, &fixture_sources("A"))?;
    let error = h
        .publish(&invalid, None)
        .expect_err("dangling endpoint accepted")
        .to_string();
    require(error.contains("dangling"), &error)?;
    require(
        h.head()?.as_deref() == Some(&r1.revision),
        "failed transaction moved HEAD",
    )?;
    target(&h, &view, "A")?;
    let only_common = vec![fixture("A", "prod")[0].clone()];
    let empty = h.prepare(
        "remove-entry",
        h.head()?,
        &only_common,
        &fixture_sources("A"),
    )?;
    let stats = h.publish(&empty, None)?;
    require(
        stats.payloads == 0 && stats.uses_closed == 1 && stats.uses_started == 0,
        "deletion rewrote payload",
    )?;
    let next = h.open_view(None, "prod", 1000)?;
    let read = h.start_read(&next)?;
    require(
        read.entities("entry")?.is_empty() && read.edges("prod::entry", "", 10)?.is_empty(),
        "removed declaration survived",
    )?;
    target(&h, &view, "A")?;
    Ok(json!({"invalid_candidate_error":error,"deletion":stats,"old_view_retained":true}))
}

pub fn concurrent_writers(root: &Path) -> Result<Value> {
    let (h, _) = initial(root)?;
    let b = prepare(&h, "writer-b", "B")?;
    let c = prepare(&h, "writer-c", "C")?;
    let barrier = Arc::new(Barrier::new(3));
    let jobs = [b, c]
        .into_iter()
        .map(|candidate| {
            let host = h.clone();
            let gate = barrier.clone();
            std::thread::spawn(move || {
                gate.wait();
                (
                    candidate.revision.clone(),
                    host.publish(&candidate, None).map_err(|e| e.to_string()),
                )
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = jobs
        .into_iter()
        .map(|t| t.join().expect("writer thread"))
        .collect::<Vec<_>>();
    require(
        results.iter().filter(|(_, r)| r.is_ok()).count() == 1,
        "CAS accepted both writers",
    )?;
    require(
        results
            .iter()
            .filter(|(_, r)| r.as_ref().is_err_and(|e| e.contains("PublishConflict")))
            .count()
            == 1,
        "loser not a CAS conflict",
    )?;
    let winner = results.iter().find(|(_, r)| r.is_ok()).ok_or("no winner")?;
    require(h.head()?.as_ref() == Some(&winner.0), "wrong winning HEAD")?;
    Ok(
        json!({"results":results.into_iter().map(|(revision,result)|json!({"revision":revision,"result":result})).collect::<Vec<_>>()}),
    )
}

fn kill_at(root: &Path, stage: &str, mode: &str) -> Result<Value> {
    let stderr = root.join(format!("worker-{stage}.stderr"));
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--worker")
        .arg(mode)
        .arg(root)
        .arg(stage)
        .stdout(Stdio::piped())
        .stderr(fs::File::create(&stderr)?)
        .spawn()?;
    let out = child.stdout.take().ok_or("child stdout missing")?;
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(out).lines() {
            match line {
                Ok(line) if line.starts_with("R0_READY") => {
                    let _ = tx.send(line);
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
    let ready = rx.recv_timeout(Duration::from_secs(30));
    if ready.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        return Err(format!(
            "worker did not reach {stage}: {}",
            fs::read_to_string(stderr)?
        )
        .into());
    }
    let status = fs::read_to_string(format!("/proc/{}/status", child.id())).unwrap_or_default();
    child.kill()?;
    let exit = child.wait()?;
    let _ = reader.join();
    use std::os::unix::process::ExitStatusExt;
    require(
        exit.signal() == Some(libc::SIGKILL),
        "worker not killed by SIGKILL",
    )?;
    Ok(json!({"barrier":ready?,"signal":exit.signal(),"status_before_kill":status}))
}
pub fn crash(root: &Path, stage: &str, double_recovery: bool) -> Result<Value> {
    let (h, r1) = initial(root)?;
    let r2 = prepare(&h, "crash-candidate", "B")?;
    let old = h.open_view(None, "prod", 1000)?;
    drop(h);
    let killed = kill_at(root, stage, "publish")?;
    let second = if double_recovery {
        Some(kill_at(root, "before-recovery-commit", "recover")?)
    } else {
        None
    };
    let h = Host::open(root)?;
    h.recover(None)?;
    h.recover(None)?;
    let committed = ["after-head-rename", "after-head"].contains(&stage);
    require(
        h.head()?.as_deref()
            == Some(if committed {
                &r2.revision
            } else {
                &r1.revision
            }),
        "unexpected recovered HEAD",
    )?;
    require(
        h.native_pending_count()? == 0,
        "pending transaction survived recovery",
    )?;
    target(&h, &old, "A")?;
    latest(&h, if committed { "B" } else { "A" })?;
    if !committed {
        require(
            h.open_view(Some(&r2.revision), "prod", 1000).is_err(),
            "aborted revision became readable",
        )?;
        let next = prepare(&h, "after-recovery-c", "C")?;
        h.publish(&next, None)?;
        latest(&h, "C")?;
        target(&h, &old, "A")?;
    } else {
        require(
            h.publish(&r2, None)?.payloads == 0,
            "committed retry imported rows",
        )?;
    }
    Ok(
        json!({"stage":stage,"sigkill":killed,"recovery_sigkill":second,"expected_committed":committed,"pending":0,"power_loss_test":false}),
    )
}

pub fn epoch(root: &Path) -> Result<Value> {
    let (h, r1) = initial(root)?;
    let old = h.open_view(None, "prod", 1000)?;
    let expected = h.build_epoch("e2")?;
    // Real publication between the initial rebuild and final cutover.
    let r2 = prepare(&h, "during-rebuild", "B")?;
    h.publish(&r2, None)?;
    let shadow = h.projection("e2")?;
    let c = shadow.connection()?;
    let unsafe_rows = shadow.edges(&c, r2.manifest.sequence, "prod", "prod::entry", "", 10)?;
    require(
        string(&unsafe_rows[0][1])? == "prod::A",
        "negative control did not expose stale epoch",
    )?;
    drop(c);
    drop(shadow);
    h.switch_epoch("e2", &expected, None)?;
    require(
        h.head()?.as_deref() == Some(&r2.revision),
        "epoch switch changed HEAD",
    )?;
    let new = h.open_view(None, "prod", 1000)?;
    require(
        new.epoch == "e2" && old.epoch == "e1",
        "route binding wrong",
    )?;
    target(&h, &old, "A")?;
    target(&h, &new, "B")?;
    require(h.collect_epoch("e1").is_err(), "leased epoch deleted")?;
    drop(h);
    let h = Host::open(root)?;
    target(&h, &old, "A")?;
    target(&h, &new, "B")?;
    h.release(&old)?;
    h.collect_epoch("e1")?;
    require(
        !h.epoch_path("e1").exists(),
        "unleased old epoch survived collection",
    )?;
    latest(&h, "B")?;
    Ok(
        json!({"negative_control_unfenced_epoch_target":"prod::A","correct_latest_target":"prod::B","old_revision":r1.revision,"new_revision":r2.revision,"old_epoch_collected_after_release":true,"restart_preserved_views":true}),
    )
}
pub fn epoch_crash(root: &Path, stage: &str) -> Result<Value> {
    let (h, _) = initial(root)?;
    let expected = h.build_epoch("e2")?;
    let next = prepare(&h, "during-rebuild", "B")?;
    h.publish(&next, None)?;
    save(&root.join("expected-route.json"), &expected)?;
    drop(h);
    let killed = kill_at(root, stage, "epoch")?;
    let h = Host::open(root)?;
    require(
        h.route()?.epoch == if stage == "before-route" { "e1" } else { "e2" },
        "route commit decision wrong",
    )?;
    latest(&h, "B")?;
    h.switch_epoch("e2", &expected, None)?;
    latest(&h, "B")?;
    let head_unchanged = h.head()?.as_deref() == Some(&next.revision);
    require(head_unchanged, "epoch crash recovery changed analysis HEAD")?;
    Ok(
        json!({"stage":stage,"sigkill":killed,"recovered_route":h.route()?,"head_unchanged":head_unchanged}),
    )
}

pub fn leases(root: &Path) -> Result<Value> {
    let (h, r1) = initial(root)?;
    let view = h.open_view(None, "prod", 10)?;
    let read = h.start_read(&view)?;
    let next = prepare(&h, "new-head", "B")?;
    h.publish(&next, None)?;
    h.advance_clock(11)?;
    require(
        h.start_read(&view).is_err(),
        "expired lease admitted a new request",
    )?;
    let protected = h.plan_gc(std::slice::from_ref(&r1.revision))?;
    require(
        protected.revisions.is_empty(),
        "active execution lost protection on expiration",
    )?;
    require(
        read.edges("prod::entry", "", 10)?[0].1 == "prod::A",
        "active old read changed",
    )?;
    drop(read);
    let gc = h.plan_gc(std::slice::from_ref(&r1.revision))?;
    require(gc.revisions.len() == 1, "expired idle version not retired")?;
    require(
        h.open_view(Some(&r1.revision), "prod", 10).is_err(),
        "retiring version admitted",
    )?;
    let removed = h.sweep(&gc)?;
    require(
        removed == 2,
        "did not collect exactly obsolete fact/evidence",
    )?;
    require(
        h.manifest(&next.revision).is_ok(),
        "collected live manifest",
    )?;
    let shared_target_payload_retained = h.payload(&next.manifest.shards["prod/targets"]).is_ok();
    require(
        shared_target_payload_retained,
        "shared canonical target payload was deleted",
    )?;
    h.audit_manifest(&next.manifest)?;
    latest(&h, "B")?;
    Ok(
        json!({"expired_cursor_rejected":true,"active_execution_protected":true,"obsolete_objects_removed":removed,"shared_target_payload_retained":shared_target_payload_retained,"canonical_manifest_audited":true,"clock":"deterministic logical test clock"}),
    )
}
pub fn gc_roots(root: &Path) -> Result<Value> {
    let (h, r1) = initial(root)?;
    let next = prepare(&h, "second", "B")?;
    h.publish(&next, None)?;
    let plan = h.plan_gc(std::slice::from_ref(&r1.revision))?;
    let candidate = prepare(&h, "reuse-before-sweep", "A")?;
    require(
        h.sweep(&plan).is_err(),
        "sweep ignored root generation change",
    )?;
    let replacement = h.plan_gc(std::slice::from_ref(&r1.revision))?;
    require(
        replacement.objects.is_empty(),
        "new candidate did not protect reused objects",
    )?;
    h.sweep(&replacement)?;
    h.publish(&candidate, None)?;
    latest(&h, "A")?;
    Ok(
        json!({"stale_plan_rejected":true,"candidate_protected_reused_payload":true,"recomputed_deletions":0}),
    )
}
pub fn admission_race(root: &Path) -> Result<Value> {
    let (h, r1) = initial(root)?;
    let next = prepare(&h, "second", "B")?;
    h.publish(&next, None)?;
    let gate = Arc::new(Barrier::new(3));
    let a = {
        let h = h.clone();
        let gate = gate.clone();
        let id = r1.revision.clone();
        std::thread::spawn(move || {
            gate.wait();
            h.open_view(Some(&id), "prod", 10)
                .map_err(|e| e.to_string())
        })
    };
    let b = {
        let h = h.clone();
        let gate = gate.clone();
        let id = r1.revision.clone();
        std::thread::spawn(move || {
            gate.wait();
            h.plan_gc(&[id]).map_err(|e| e.to_string())
        })
    };
    gate.wait();
    let admitted = a.join().expect("reader thread");
    let plan = b.join().expect("GC thread")?;
    let winner = if let Ok(view) = admitted {
        require(
            plan.revisions.is_empty(),
            "reader admitted and version retired",
        )?;
        target(&h, &view, "A")?;
        "reader"
    } else {
        require(
            plan.revisions.len() == 1,
            "reader rejected without retirement",
        )?;
        h.sweep(&plan)?;
        "gc"
    };
    latest(&h, "B")?;
    Ok(json!({"barrier_controlled_threads":true,"winner":winner,"illegal_outcome":false}))
}

pub fn ownership_and_cancel(root: &Path) -> Result<Value> {
    let (h, _) = initial(root)?;
    let p = h.current_projection()?;
    require(
        Host::open_unrecovered(root).is_err(),
        "application allowed a second host",
    )?;
    let status = Command::new(std::env::current_exe()?)
        .arg("--ownership-worker")
        .arg(h.epoch_path("e1"))
        .output()?;
    require(
        status.status.success(),
        "cross-process duplicate owner allowed",
    )?;
    // Observe native behavior without ever writing through duplicate owners.
    // This negative control uses a separate file, not the host database.
    let duplicate_path = root.join("same-process-negative-control.lbdb");
    let first = lbug::Database::new(&duplicate_path, config())?;
    let duplicate = lbug::Database::new(&duplicate_path, config());
    let native_same_process_second_open_allowed = duplicate.is_ok();
    drop(duplicate);
    drop(first);
    let expensive = "UNWIND range(1,10000) AS i UNWIND range(1,100000) AS j RETURN sum((i*j)%97)";
    let a = p.connection()?;
    let b = p.connection()?;
    a.set_query_timeout(1);
    let timeout = a
        .query(expensive)
        .err()
        .ok_or("expensive query did not time out")?
        .to_string();
    require(
        timeout.to_lowercase().contains("interrupt") || timeout.to_lowercase().contains("timeout"),
        format!("not a timeout: {timeout}"),
    )?;
    a.set_query_timeout(10_000);
    require(
        p.count(&a, "RETURN 42")? == 42,
        "connection not reusable after timeout",
    )?;
    let interrupted = std::thread::scope(|scope| -> Result<String> {
        let (tx, rx) = mpsc::channel();
        let active = &a;
        let worker = scope.spawn(move || {
            let _ = tx.send(());
            active
                .query(expensive)
                .map(|r| r.to_string())
                .map_err(|e| e.to_string())
        });
        rx.recv_timeout(Duration::from_secs(1))?;
        // Ladybug has no execution-start callback in this binding. The short delay
        // belongs only to this cancellation probe, not the protocol race tests.
        std::thread::sleep(Duration::from_millis(20));
        a.interrupt()?;
        require(
            p.count(&b, "RETURN 7")? == 7,
            "other connection interrupted",
        )?;
        worker
            .join()
            .map_err(|_| "query thread panicked")?
            .map_or_else(Ok, |_| Err("query completed before interrupt".into()))
    })?;
    require(
        interrupted.to_lowercase().contains("interrupt"),
        format!("not an interrupt: {interrupted}"),
    )?;
    require(
        p.count(&a, "RETURN 9")? == 9,
        "connection not reusable after interrupt",
    )?;
    p.exec(&a, "BEGIN TRANSACTION READ ONLY")?;
    let readonly = p
        .exec(&a, "CREATE (:EntitySlot {id:'forbidden',context:'prod'})")
        .is_err();
    let _ = p.exec(&a, "ROLLBACK");
    require(readonly, "read-only transaction accepted write")?;
    Ok(
        json!({"application_second_host_rejected":true,"native_same_process_second_open_allowed":native_same_process_second_open_allowed,"cross_process_owner_rejected":true,"timeout_error":timeout,"interrupt_error":interrupted,"other_connection_survived":true,"connection_reusable":true,"readonly_write_rejected":true}),
    )
}

pub fn mvcc_reader(root: &Path) -> Result<Value> {
    let (h, _) = initial(root)?;
    let old = h.open_view(None, "prod", 1000)?;
    let p = h.current_projection()?;
    let c = p.connection()?;
    p.exec(&c, "BEGIN TRANSACTION READ ONLY")?;
    let first = p.edges(&c, 1, "prod", "prod::entry", "", 10)?;
    let next = prepare(&h, "writer", "B")?;
    let writer = {
        let h = h.clone();
        std::thread::spawn(move || h.publish(&next, None).map_err(|e| e.to_string()))
    };
    let stats = writer.join().expect("writer thread")?;
    let second = p.edges(&c, 1, "prod", "prod::entry", "", 10)?;
    require(
        string(&first[0][1])? == "prod::A" && string(&second[0][1])? == "prod::A",
        "reader transaction mixed versions",
    )?;
    p.exec(&c, "COMMIT")?;
    latest(&h, "B")?;
    target(&h, &old, "A")?;
    Ok(
        json!({"reader_held_open_during_write":true,"writer":stats,"old_reader_target":"prod::A","new_reader_target":"prod::B"}),
    )
}

pub fn pagination(root: &Path) -> Result<Value> {
    let h = Host::create(root)?;
    let mut nodes = vec![Node {
        id: "prod::hub".into(),
        name: "hub".into(),
        doc: "fanout".into(),
    }];
    let mut edges = vec![];
    for i in 0..1500 {
        let id = format!("prod::n{i:04}");
        nodes.push(Node {
            id: id.clone(),
            name: id.clone(),
            doc: String::new(),
        });
        edges.push(Edge {
            from: "prod::hub".into(),
            to: id,
            occurrence: format!("macro-spelling-{i:04}"),
        });
    }
    let source = b"controlled synthetic fanout graph, not a compiler export".to_vec();
    let payload = Payload {
        shard: "prod/fanout".into(),
        context: "prod".into(),
        nodes,
        edges,
        source: digest(&source),
    };
    let candidate = h.prepare("fanout", None, &[payload], &[source])?;
    h.publish(&candidate, None)?;
    let view = h.open_view(None, "prod", 1000)?;
    let read = h.start_read(&view)?;
    let mut after = String::new();
    let mut seen = BTreeSet::new();
    let mut pages = 0;
    loop {
        let rows = read.edges("prod::hub", &after, 7)?;
        if rows.is_empty() {
            break;
        }
        pages += 1;
        for (id, target) in rows {
            require(seen.insert(target), "pagination duplicate")?;
            after = id;
        }
    }
    require(seen.len() == 1500, "pagination lost neighbors")?;
    let sql = Projection::edges_sql(view.sequence, "prod", "prod::hub", "", 7, true);
    let plan = read.projection.profile(&sql)?;
    durable(&root.join("first-page-profile.txt"), plan.as_bytes())?;
    let mut milliseconds = vec![];
    for _ in 0..30 {
        let t = Instant::now();
        require(
            read.edges("prod::hub", "", 7)?.len() == 7,
            "first page size wrong",
        )?;
        milliseconds.push(t.elapsed().as_secs_f64() * 1000.);
    }
    milliseconds.sort_by(f64::total_cmp);
    let connection = read.projection.connection()?;
    let parameterized = sql.replace("e.id > \"\"", "e.id > $after");
    let t = Instant::now();
    let mut prepared = connection.prepare(&parameterized)?;
    let prepare_ms = t.elapsed().as_secs_f64() * 1000.;
    let mut prepared_ms = vec![];
    for _ in 0..30 {
        let t = Instant::now();
        let rows = connection
            .execute(
                &mut prepared,
                vec![("after", lbug::Value::String(String::new()))],
            )?
            .collect::<Vec<_>>();
        require(rows.len() == 7, "prepared page wrong")?;
        prepared_ms.push(t.elapsed().as_secs_f64() * 1000.);
    }
    prepared_ms.sort_by(f64::total_cmp);
    let hash = &candidate.manifest.shards["prod/fanout"].hash;
    let mut indexed = BTreeSet::new();
    let mut ordinal = 0;
    loop {
        let rows = read.adjacency_page(hash, "prod::hub", ordinal)?;
        if rows.is_empty() {
            break;
        }
        for (_, target) in rows {
            require(indexed.insert(target), "duplicate indexed adjacency")?;
        }
        ordinal += 1;
    }
    require(indexed == seen, "native and indexed adjacency sets differ")?;
    let point_sql = format!(
        "MATCH (p:AdjacencyPage {{id:{}}}) RETURN p.payload,p.context,p.subject,p.items",
        crate::projection::quote(&Projection::adjacency_key(hash, "prod::hub", 0))
    );
    let point_plan = read.projection.profile(&point_sql)?;
    durable(
        &root.join("adjacency-page-profile.txt"),
        point_plan.as_bytes(),
    )?;
    require(
        point_plan.contains("INDEX_SCAN") || point_plan.contains("PRIMARY_KEY_SCAN"),
        "adjacency point query did not use a native primary-key scan",
    )?;
    let mut bounded_ms = vec![];
    for _ in 0..30 {
        let t = Instant::now();
        require(
            read.adjacency_page(hash, "prod::hub", 0)?.len() == 7,
            "indexed first page wrong",
        )?;
        bounded_ms.push(t.elapsed().as_secs_f64() * 1000.);
    }
    bounded_ms.sort_by(f64::total_cmp);
    Ok(
        json!({"neighbors":seen.len(),"pages":pages,"page_size":7,"samples":30,"p50_ms":milliseconds[14],"p95_ms":milliseconds[28],"samples_ms":milliseconds,"plan":"first-page-profile.txt","native_scan_bound_proven":false,
        "prepared_join":{"prepare_ms":prepare_ms,"p50_ms":prepared_ms[14],"p95_ms":prepared_ms[28],"samples_ms":prepared_ms},
        "adjacency_page":{"rows_per_page":7,"max_items_bytes":16384,"pages":ordinal,"p50_ms":bounded_ms[14],"p95_ms":bounded_ms[28],"samples_ms":bounded_ms,"plan":"adjacency-page-profile.txt","native_primary_index_scan":true,"scope":"already-resolved FactRef in one relation shard; not full symbol-to-multiple-shards lookup"},
        "note":"native join scans more candidates than returned; prepared statements reduce planning cost, while primary-key adjacency pages bound this resolved-reference read"}),
    )
}

pub fn source_integrity(root: &Path) -> Result<Value> {
    let (h, r1) = initial(root)?;
    let view = h.open_view(None, "prod", 1000)?;
    let source = root
        .join("evidence")
        .join(&r1.manifest.shards["prod/entry"].source);
    fs::write(&source, b"tampered frozen evidence")?;
    let read = h.start_read(&view)?;
    require(
        read.source("prod/entry").is_err(),
        "corrupt evidence returned",
    )?;
    fs::remove_file(source)?;
    require(
        read.source("prod/entry").is_err(),
        "missing evidence silently replaced",
    )?;
    Ok(json!({"tamper_rejected":true,"missing_rejected":true,"live_workspace_fallback":false}))
}

pub fn worker(mode: &str, root: &Path, stage: &str) -> Result<()> {
    let h = Host::open_unrecovered(root)?;
    match mode {
        "publish" => {
            let c: Candidate = load(&root.join("candidates/crash-candidate.json"))?;
            h.publish(&c, Some(stage))?;
        }
        "recover" => h.recover(Some(stage))?,
        "epoch" => {
            let expected: Route = load(&root.join("expected-route.json"))?;
            h.switch_epoch("e2", &expected, Some(stage))?;
        }
        _ => return Err("unknown worker mode".into()),
    }
    Err("crash barrier not reached".into())
}
