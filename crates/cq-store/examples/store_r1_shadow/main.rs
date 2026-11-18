//! R1 batch 1: scope-completion contracts only. No LocalStore or Ladybug writes.
mod scope;
use scope::*;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn check(ok: bool, message: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(message.into()) }
}
fn key(unit: &str, context: &str) -> ScopeKey {
    ScopeKey {
        unit: unit.into(),
        context: context.into(),
        family: FactFamily::Relation,
    }
}
fn facts(text: &str) -> BTreeMap<String, ContentRef> {
    BTreeMap::from([(
        "file/0".into(),
        ContentRef {
            hash: hash(text.as_bytes()),
            bytes: text.len() as u64,
        },
    )])
}
fn receipt(scope: &ScopeKey, status: Completion, mode: Output) -> Receipt {
    Receipt::new(scope.clone(), status, mode).expect("valid fixture receipt")
}
fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    check(
        args.len() == 2 && args[0] == "--output",
        "usage: store_r1_shadow --output FRESH_DIRECTORY",
    )?;
    let out = PathBuf::from(&args[1]);
    check(!out.exists(), "fresh directory required")?;
    std::fs::create_dir_all(&out)?;
    let a = key("unit-a", "prod");
    let b = key("unit-b", "prod");
    let empty = Shadow::default();
    let selected = BTreeSet::from([a.clone()]);
    let no_invalidations = BTreeSet::new();
    let first = receipt(&a, Completion::Complete, Output::Full(facts("first")));
    let base = apply(
        &empty,
        &selected,
        true,
        std::slice::from_ref(&first),
        Policy::Strict,
        &no_invalidations,
    )?;
    let mut results = vec![];
    macro_rules! record {($name:expr,$condition:expr)=>{{let ok=$condition;check(ok,$name)?;results.push(json!({"case":$name,"passed":ok}));}};}
    record!("complete_full_inventory", base.scopes[&a] == facts("first"));
    let cleared = apply(
        &base,
        &selected,
        true,
        &[receipt(
            &a,
            Completion::Complete,
            Output::Full(BTreeMap::new()),
        )],
        Policy::Strict,
        &no_invalidations,
    )?;
    record!(
        "complete_empty_removes_old_facts",
        cleared.scopes[&a].is_empty()
    );
    record!(
        "missing_receipt_is_not_empty",
        apply(
            &base,
            &selected,
            true,
            &[],
            Policy::Strict,
            &no_invalidations
        )
        .is_err()
    );
    record!(
        "incomplete_selection_cannot_remove_units",
        apply(
            &base,
            &BTreeSet::new(),
            false,
            &[],
            Policy::Strict,
            &no_invalidations
        )
        .is_err()
    );
    record!(
        "unexpected_scope_rejected",
        apply(
            &base,
            &selected,
            true,
            &[receipt(
                &b,
                Completion::Complete,
                Output::Full(facts("other"))
            )],
            Policy::Strict,
            &no_invalidations
        )
        .is_err()
    );
    for (name, status) in [
        ("partial_rejected_by_default", Completion::Partial),
        ("failed_rejected_by_default", Completion::Failed),
    ] {
        record!(
            name,
            apply(
                &base,
                &selected,
                true,
                &[receipt(&a, status, Output::Full(facts("subset")))],
                Policy::Strict,
                &no_invalidations
            )
            .is_err()
        );
    }
    let two_selected = BTreeSet::from([a.clone(), b.clone()]);
    let two = apply(
        &empty,
        &two_selected,
        true,
        &[
            first.clone(),
            receipt(&b, Completion::Complete, Output::Full(facts("independent"))),
        ],
        Policy::Strict,
        &no_invalidations,
    )?;
    let invalidated = BTreeSet::from([a.clone()]);
    let partial = apply(
        &two,
        &two_selected,
        true,
        &[
            receipt(&a, Completion::Failed, Output::Full(BTreeMap::new())),
            receipt(
                &b,
                Completion::Complete,
                Output::Reuse {
                    base: two.scope_root(&b)?,
                    input_witness: hash(b"verified-input-closure"),
                },
            ),
        ],
        Policy::AllowPartial,
        &invalidated,
    )?;
    record!(
        "explicit_partial_retires_invalidated_old_scope",
        partial.partial
            && !partial.scopes.contains_key(&a)
            && partial.scopes[&b] == two.scopes[&b]
            && partial.gaps == BTreeSet::from([a.clone()])
    );
    let reuse = receipt(
        &a,
        Completion::Complete,
        Output::Reuse {
            base: base.scope_root(&a)?,
            input_witness: hash(b"witness"),
        },
    );
    record!(
        "invalidated_scope_cannot_reuse_old_output",
        apply(
            &base,
            &selected,
            true,
            &[reuse],
            Policy::AllowPartial,
            &invalidated
        )
        .is_err()
    );
    let delta = receipt(
        &a,
        Completion::Complete,
        Output::Delta {
            base: base.scope_root(&a)?,
            upserts: BTreeMap::from([(
                "file/1".into(),
                ContentRef {
                    hash: hash(b"next"),
                    bytes: 4,
                },
            )]),
            deletes: BTreeSet::from(["file/0".into()]),
        },
    );
    let changed = apply(
        &base,
        &selected,
        true,
        &[delta],
        Policy::Strict,
        &no_invalidations,
    )?;
    record!(
        "delta_applies_explicit_upserts_and_deletes",
        changed.scopes[&a].len() == 1 && changed.scopes[&a].contains_key("file/1")
    );
    let wrong = receipt(
        &a,
        Completion::Complete,
        Output::Delta {
            base: hash(b"wrong-base"),
            upserts: BTreeMap::new(),
            deletes: BTreeSet::new(),
        },
    );
    record!(
        "stale_delta_base_rejected",
        apply(
            &base,
            &selected,
            true,
            &[wrong],
            Policy::Strict,
            &no_invalidations
        )
        .is_err()
    );
    let unknown = receipt(
        &a,
        Completion::Complete,
        Output::Delta {
            base: base.scope_root(&a)?,
            upserts: BTreeMap::new(),
            deletes: BTreeSet::from(["absent".into()]),
        },
    );
    record!(
        "deleting_unknown_shard_rejected",
        apply(
            &base,
            &selected,
            true,
            &[unknown],
            Policy::Strict,
            &no_invalidations
        )
        .is_err()
    );
    record!(
        "identical_receipt_retry_idempotent",
        apply(
            &base,
            &selected,
            true,
            &[first.clone(), first.clone()],
            Policy::Strict,
            &no_invalidations
        )?
        .root()?
            == base.root()?
    );
    let conflict = receipt(&a, Completion::Complete, Output::Full(facts("different")));
    record!(
        "conflicting_receipt_retry_rejected",
        apply(
            &base,
            &selected,
            true,
            &[first.clone(), conflict],
            Policy::Strict,
            &no_invalidations
        )
        .is_err()
    );
    let mut bad = first.clone();
    bad.digest = hash(b"forged");
    record!(
        "receipt_digest_mismatch_rejected",
        apply(
            &base,
            &selected,
            true,
            &[bad],
            Policy::Strict,
            &no_invalidations
        )
        .is_err()
    );
    let malformed = receipt(
        &a,
        Completion::Complete,
        Output::Full(BTreeMap::from([(
            "file".into(),
            ContentRef {
                hash: "not-sha256".into(),
                bytes: 1,
            },
        )])),
    );
    record!(
        "malformed_content_ref_rejected",
        apply(
            &base,
            &selected,
            true,
            &[malformed],
            Policy::Strict,
            &no_invalidations
        )
        .is_err()
    );
    let test = key("unit-a", "test");
    let variants = BTreeSet::from([a.clone(), test.clone()]);
    let isolated = apply(
        &empty,
        &variants,
        true,
        &[
            first,
            receipt(
                &test,
                Completion::Complete,
                Output::Full(facts("test-only")),
            ),
        ],
        Policy::Strict,
        &no_invalidations,
    )?;
    record!(
        "same_unit_different_contexts_remain_distinct",
        isolated.scopes.len() == 2 && isolated.scope_root(&a)? != isolated.scope_root(&test)?
    );
    let removed = apply(
        &two,
        &selected,
        true,
        &[receipt(
            &a,
            Completion::Complete,
            Output::Full(facts("first")),
        )],
        Policy::Strict,
        &no_invalidations,
    )?;
    record!(
        "authoritative_selection_removes_exited_unit",
        !removed.scopes.contains_key(&b)
    );
    let summary = json!({"stage":"R1 batch 1 isolated shadow contracts","passed":results.len(),"total":results.len(),"cases":results,"partial_shadow_example":partial.as_json()?,"limits":["no production store reads or writes","references and input witnesses are protocol fixtures, not compiler-observed proofs","fact-page IO, source blocks and persistent manifest tree are not implemented in this batch","partial conservatively omits incomplete scopes; no per-fact partial merge"]});
    std::fs::write(
        out.join("results.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;
    println!(
        "R1 SHADOW CONTRACTS {}/{}",
        summary["passed"], summary["total"]
    );
    Ok(())
}
