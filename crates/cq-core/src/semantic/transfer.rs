//! Real full/delta/reuse ingestion. Payload validation and base application are
//! independent of storage and do not need a project-wide semantic snapshot.
use super::{
    production::{self, Production, Record, ScopeDescriptor},
    scope::*,
};
use crate::{Error, Result, model::digest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone)]
pub struct BaseScope {
    pub descriptor: ScopeDescriptor,
    pub records: BTreeMap<String, ContentRef>,
}
#[derive(Clone)]
pub struct BaseSummary {
    pub descriptor: ScopeDescriptor,
    pub base_root: String,
}
pub trait Base {
    /// Only summaries bound to a previously validated immutable revision are eligible.
    fn summary(&self, key: &ScopeKey) -> Result<Option<BaseSummary>> {
        self.scope(key)?
            .map(|v| {
                if v.descriptor.digest != state_digest(&v.records)?
                    || v.descriptor.rows != v.records.len() as u64
                {
                    return Err(Error::new("InvalidScopeBase", "summary digest/count"));
                }
                Ok(BaseSummary {
                    base_root: base_root(key, &v.records)?,
                    descriptor: v.descriptor,
                })
            })
            .transpose()
    }
    fn revision(&self) -> Option<&str>;
    fn keys(&self) -> Result<Vec<ScopeKey>>;
    fn scope(&self, key: &ScopeKey) -> Result<Option<BaseScope>>;
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Begin {
    pub descriptor: ScopeDescriptor,
    pub base_revision: Option<String>,
    pub receipt: Receipt,
}
pub trait Sink {
    fn begin(&mut self, begin: &Begin) -> Result<()>;
    fn batch(&mut self, key: &ScopeKey, sequence: u64, records: &[Record]) -> Result<()>;
    fn end(&mut self, key: &ScopeKey, batches: u64) -> Result<()>;
    fn finish(&mut self) -> Result<()>;
}
pub fn state_digest(records: &BTreeMap<String, ContentRef>) -> Result<String> {
    state_digest_sorted(records.iter().map(|(k, r)| (k.as_str(), r)))
}
/// Same scope seal without allocating a second complete map. The caller must
/// supply strictly ordered unique keys, which is checked before hashing.
pub fn state_digest_sorted<'a>(
    records: impl IntoIterator<Item = (&'a str, &'a ContentRef)>,
) -> Result<String> {
    let mut h = Sha256::new();
    let mut previous = None;
    for (key, r) in records {
        if previous.is_some_and(|p| p >= key) {
            return Err(Error::new(
                "InvalidScopeOrder",
                "scope members must be ordered and unique",
            ));
        }
        previous = Some(key);
        if r.hash.len() != 64
            || !r
                .hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::new("InvalidContentRef", "noncanonical digest"));
        }
        h.update((key.len() as u64).to_le_bytes());
        h.update(key.as_bytes());
        h.update(r.bytes.to_le_bytes());
        let mut bytes = [0; 32];
        for (i, v) in bytes.iter_mut().enumerate() {
            *v = u8::from_str_radix(&r.hash[i * 2..i * 2 + 2], 16)
                .map_err(|_| Error::new("InvalidContentRef", "invalid hash"))?;
        }
        h.update(bytes);
    }
    Ok(format!("{:x}", h.finalize()))
}
pub fn base_root(key: &ScopeKey, facts: &BTreeMap<String, ContentRef>) -> Result<String> {
    Ok(digest(&serde_json::to_vec(&(1, key, facts))?))
}
pub fn unchanged_plan(descriptor: &ScopeDescriptor, base: &dyn Base) -> Result<Option<Begin>> {
    let Some(old) = base.summary(&descriptor.key)? else {
        return Ok(None);
    };
    if descriptor.completion != Completion::Complete
        || old.descriptor.completion != Completion::Complete
        || old.descriptor.key != descriptor.key
        || old.descriptor.digest != descriptor.digest
        || old.descriptor.rows != descriptor.rows
    {
        return Ok(None);
    }
    let output = if old.descriptor.input_witness == descriptor.input_witness {
        Output::Reuse {
            base: old.base_root,
            input_witness: descriptor.input_witness.clone(),
        }
    } else {
        Output::Delta {
            base: old.base_root,
            upserts: BTreeMap::new(),
            deletes: BTreeSet::new(),
        }
    };
    Ok(Some(Begin {
        descriptor: descriptor.clone(),
        base_revision: base.revision().map(str::to_owned),
        receipt: Receipt::new(descriptor.key.clone(), descriptor.completion, output)?,
    }))
}
pub fn compact_match(b: &Begin, old: &BaseSummary) -> bool {
    let s = &b.descriptor;
    s.completion == Completion::Complete
        && old.descriptor.completion == Completion::Complete
        && old.descriptor.key == s.key
        && old.descriptor.rows == s.rows
        && old.descriptor.digest == s.digest
        && match &b.receipt.output {
            Output::Reuse {
                base,
                input_witness,
            } => {
                *base == old.base_root
                    && *input_witness == s.input_witness
                    && *input_witness == old.descriptor.input_witness
            }
            Output::Delta {
                base,
                upserts,
                deletes,
            } => *base == old.base_root && upserts.is_empty() && deletes.is_empty(),
            _ => false,
        }
}
pub fn plan(
    descriptor: &ScopeDescriptor,
    current: BTreeMap<String, ContentRef>,
    base: &dyn Base,
) -> Result<Begin> {
    if let Some(b) = unchanged_plan(descriptor, base)? {
        return Ok(b);
    }
    let previous = base.scope(&descriptor.key)?;
    let output = match previous {
        Some(old)
            if old.descriptor.completion == Completion::Complete
                && descriptor.completion == Completion::Complete =>
        {
            let root = base_root(&descriptor.key, &old.records)?;
            if old.descriptor.input_witness == descriptor.input_witness && old.records == current {
                Output::Reuse {
                    base: root,
                    input_witness: descriptor.input_witness.clone(),
                }
            } else {
                Output::Delta {
                    base: root,
                    upserts: current
                        .iter()
                        .filter(|(k, r)| old.records.get(*k) != Some(r))
                        .map(|(k, r)| (k.clone(), r.clone()))
                        .collect(),
                    deletes: old
                        .records
                        .keys()
                        .filter(|k| !current.contains_key(*k))
                        .cloned()
                        .collect(),
                }
            }
        }
        _ => Output::Full(current),
    };
    Ok(Begin {
        descriptor: descriptor.clone(),
        base_revision: base.revision().map(str::to_owned),
        receipt: Receipt::new(descriptor.key.clone(), descriptor.completion, output)?,
    })
}
struct Active {
    begin: Begin,
    expected: BTreeMap<String, ContentRef>,
    next: BTreeMap<String, ContentRef>,
    received: BTreeSet<String>,
    sequence: u64,
    last: Option<String>,
    proven_rows: Option<u64>,
}
pub struct Validator<'a> {
    base: &'a dyn Base,
    selected: BTreeMap<ScopeKey, ScopeDescriptor>,
    partial: bool,
    seen: BTreeSet<ScopeKey>,
    active: Option<Active>,
    poisoned: bool,
}
impl<'a> Validator<'a> {
    pub fn new(p: &Production, base: &'a dyn Base) -> Result<Self> {
        if p.version != 1 || !p.selection_complete && !p.partial {
            return Err(Error::new(
                "IncompleteSelection",
                "selection needs proof or explicit partial quarantine",
            ));
        }
        let mut selected = BTreeMap::new();
        for s in &p.scopes {
            if s.input_witness.len() != 64
                || !s.input_witness.bytes().all(|b| b.is_ascii_hexdigit())
                || s.key.context != digest(s.key.unit.as_bytes())
            {
                return Err(Error::new(
                    "InvalidScopeReceipt",
                    "unbound input/context witness",
                ));
            }
            if selected.insert(s.key.clone(), s.clone()).is_some() {
                return Err(Error::new("ConflictingScopeReceipt", "duplicate selection"));
            }
        }
        Ok(Self {
            base,
            selected,
            partial: p.partial,
            seen: BTreeSet::new(),
            active: None,
            poisoned: false,
        })
    }
    fn checked(&mut self, f: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        if self.poisoned {
            return Err(Error::new(
                "InvalidProductionTransaction",
                "discard failed transfer",
            ));
        }
        let result = f(self);
        self.poisoned = result.is_err();
        result
    }
    fn start(&mut self, b: &Begin) -> Result<()> {
        let s = &b.descriptor;
        if self.active.is_some()
            || !self.seen.insert(s.key.clone())
            || self.selected.get(&s.key).is_none_or(|expected| {
                serde_json::to_value(expected).ok() != serde_json::to_value(s).ok()
            })
            || b.receipt.scope != s.key
            || b.receipt.status != s.completion
            || b.base_revision.as_deref() != self.base.revision()
        {
            return Err(Error::new(
                "StaleScopeBase",
                "scope, receipt or base revision differs",
            ));
        }
        if let Some(old) = self.base.summary(&s.key)?
            && compact_match(b, &old)
        {
            if Receipt::new(
                b.receipt.scope.clone(),
                b.receipt.status,
                b.receipt.output.clone(),
            )?
            .digest
                != b.receipt.digest
            {
                return Err(Error::new("ReceiptDigestMismatch", "compact receipt"));
            }
            self.active = Some(Active {
                begin: b.clone(),
                expected: BTreeMap::new(),
                next: BTreeMap::new(),
                received: BTreeSet::new(),
                sequence: 0,
                last: None,
                proven_rows: Some(s.rows),
            });
            return Ok(());
        }
        let old = self.base.scope(&s.key)?;
        let mut shadow = Shadow::default();
        if let Some(old) = &old {
            if old.descriptor.digest != state_digest(&old.records)?
                || old.descriptor.rows != old.records.len() as u64
            {
                return Err(Error::new(
                    "InvalidScopeBase",
                    "base members differ from receipt",
                ));
            }
            shadow.scopes.insert(s.key.clone(), old.records.clone());
        }
        if let Output::Reuse { input_witness, .. } = &b.receipt.output
            && old.as_ref().is_none_or(|old| {
                old.descriptor.completion != Completion::Complete
                    || old.descriptor.input_witness != *input_witness
                    || s.input_witness != *input_witness
            })
        {
            return Err(Error::new(
                "InvalidReuseProof",
                "input witness does not match validated base",
            ));
        }
        let selected = BTreeSet::from([s.key.clone()]);
        let applied = apply(
            &shadow,
            &selected,
            true,
            std::slice::from_ref(&b.receipt),
            if self.partial {
                Policy::AllowPartial
            } else {
                Policy::Strict
            },
            &BTreeSet::new(),
        )?;
        let next = applied.scopes.get(&s.key).cloned().unwrap_or_default();
        if state_digest(&next)? != s.digest || next.len() as u64 != s.rows {
            return Err(Error::new(
                "ScopeResultMismatch",
                "sparse update does not produce declared complete scope",
            ));
        }
        let expected = if s.completion != Completion::Complete {
            BTreeMap::new()
        } else {
            match &b.receipt.output {
                Output::Full(r) => r.clone(),
                Output::Delta { upserts, .. } => upserts.clone(),
                Output::Reuse { .. } => BTreeMap::new(),
            }
        };
        self.active = Some(Active {
            begin: b.clone(),
            expected,
            next,
            received: BTreeSet::new(),
            sequence: 0,
            last: None,
            proven_rows: None,
        });
        Ok(())
    }
    fn records(&mut self, key: &ScopeKey, sequence: u64, records: &[Record]) -> Result<()> {
        let a = self
            .active
            .as_mut()
            .ok_or_else(|| Error::new("MissingScopeBegin", "payload before receipt"))?;
        if &a.begin.descriptor.key != key
            || sequence != a.sequence
            || records.is_empty()
            || records.len() > production::BATCH_ROWS
        {
            return Err(Error::new("InvalidBatch", "scope/sequence/row bound"));
        }
        let prepared = crate::compute::map(records.iter().collect(), |row| {
            let value = serde_json::to_vec(&row.value)?;
            let reference = ContentRef {
                hash: digest(&value),
                bytes: value.len() as u64,
            };
            let expected_key = crate::ports::facts::fact_key(
                production::family_name(&key.family),
                row.value["id"]
                    .as_str()
                    .ok_or_else(|| Error::new("InvalidIdentity", "missing record id"))?,
            );
            Ok((value.len(), reference, expected_key))
        })?;
        let mut bytes = 0;
        for (row, (length, reference, expected_key)) in records.iter().zip(prepared) {
            bytes += length;
            if length > production::RECORD_BYTES
                || row.key != expected_key
                || a.expected.get(&row.key) != Some(&reference)
                || !a.received.insert(row.key.clone())
                || a.last.as_ref().is_some_and(|last| last >= &row.key)
            {
                return Err(Error::new(
                    "InvalidTransferPayload",
                    "unexpected, duplicate, unordered or corrupt payload",
                ));
            }
            a.last = Some(row.key.clone());
        }
        if bytes > production::BATCH_BYTES && records.len() != 1 {
            return Err(Error::new("InvalidBatch", "byte bound"));
        }
        a.sequence += 1;
        Ok(())
    }
    fn close(&mut self, key: &ScopeKey, batches: u64) -> Result<()> {
        let a = self
            .active
            .take()
            .ok_or_else(|| Error::new("MissingScopeBegin", "end before begin"))?;
        if &a.begin.descriptor.key != key
            || a.sequence != batches
            || a.received.len() != a.expected.len()
            || a.proven_rows.unwrap_or(a.next.len() as u64) != a.begin.descriptor.rows
        {
            return Err(Error::new(
                "IncompleteScope",
                "missing payload or incorrect completion",
            ));
        }
        Ok(())
    }
}
impl Sink for Validator<'_> {
    fn begin(&mut self, b: &Begin) -> Result<()> {
        self.checked(|v| v.start(b))
    }
    fn batch(&mut self, k: &ScopeKey, n: u64, r: &[Record]) -> Result<()> {
        self.checked(|v| v.records(k, n, r))
    }
    fn end(&mut self, k: &ScopeKey, n: u64) -> Result<()> {
        self.checked(|v| v.close(k, n))
    }
    fn finish(&mut self) -> Result<()> {
        self.checked(|v| {
            if v.active.is_none() && v.seen.len() == v.selected.len() {
                Ok(())
            } else {
                Err(Error::new("MissingScopeReceipt", "unclosed/missing scope"))
            }
        })
    }
}
