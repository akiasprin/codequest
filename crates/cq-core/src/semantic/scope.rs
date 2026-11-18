//! Language-neutral producer scope completion, delta base and partial policies.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub type Result<T> = crate::Result<T>;
pub fn hash(bytes: &[u8]) -> String {
    crate::model::digest(bytes)
}
fn require(ok: bool, message: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(crate::Error::new(message, "scope contract violated"))
    }
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit())
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FactFamily {
    Entity,
    Body,
    Occurrence,
    Relation,
    Document,
    Metadata,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ScopeKey {
    pub unit: String,
    pub context: String,
    pub family: FactFamily,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRef {
    pub hash: String,
    pub bytes: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Completion {
    Complete,
    Partial,
    Failed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Output {
    Full(BTreeMap<String, ContentRef>),
    Delta {
        base: String,
        upserts: BTreeMap<String, ContentRef>,
        deletes: BTreeSet<String>,
    },
    Reuse {
        base: String,
        input_witness: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub scope: ScopeKey,
    pub status: Completion,
    pub output: Output,
    pub digest: String,
}
impl Receipt {
    pub fn new(scope: ScopeKey, status: Completion, output: Output) -> Result<Self> {
        let mut receipt = Self {
            scope,
            status,
            output,
            digest: String::new(),
        };
        receipt.digest = receipt.seal()?;
        Ok(receipt)
    }
    fn seal(&self) -> Result<String> {
        Ok(hash(&serde_json::to_vec(&(
            1,
            &self.scope,
            self.status,
            &self.output,
        ))?))
    }
}
#[derive(Clone, Copy)]
pub enum Policy {
    Strict,
    AllowPartial,
}
#[derive(Default)]
pub struct Shadow {
    pub scopes: BTreeMap<ScopeKey, BTreeMap<String, ContentRef>>,
    pub partial: bool,
    pub gaps: BTreeSet<ScopeKey>,
}
impl Shadow {
    pub fn scope_root(&self, key: &ScopeKey) -> Result<String> {
        let facts = self
            .scopes
            .get(key)
            .ok_or_else(|| crate::Error::new("MissingBaseScope", "scope has no verified base"))?;
        Ok(hash(&serde_json::to_vec(&(1, key, facts))?))
    }
    pub fn as_json(&self) -> Result<Value> {
        let scopes = self
            .scopes
            .iter()
            .map(|(key, facts)| json!({"scope":key,"facts":facts}))
            .collect::<Vec<_>>();
        Ok(json!({"schema":1,"scopes":scopes,"partial":self.partial,"gaps":self.gaps}))
    }
    pub fn root(&self) -> Result<String> {
        Ok(hash(&serde_json::to_vec(&self.as_json()?)?))
    }
}
fn validate(facts: &BTreeMap<String, ContentRef>) -> Result<()> {
    for (key, r) in facts {
        require(
            !key.is_empty() && valid_hash(&r.hash) && r.bytes <= 8 * 1024 * 1024,
            "InvalidContentRef",
        )?;
    }
    Ok(())
}
pub fn apply(
    base: &Shadow,
    selected: &BTreeSet<ScopeKey>,
    selection_complete: bool,
    receipts: &[Receipt],
    policy: Policy,
    invalidated: &BTreeSet<ScopeKey>,
) -> Result<Shadow> {
    require(selection_complete, "IncompleteSelection")?;
    let mut by_scope: BTreeMap<ScopeKey, &Receipt> = BTreeMap::new();
    for receipt in receipts {
        require(receipt.digest == receipt.seal()?, "ReceiptDigestMismatch")?;
        require(selected.contains(&receipt.scope), "UnexpectedScope")?;
        if let Some(previous) = by_scope.insert(receipt.scope.clone(), receipt) {
            require(previous.digest == receipt.digest, "ConflictingScopeReceipt")?;
        }
    }
    require(
        by_scope.keys().cloned().collect::<BTreeSet<_>>() == *selected,
        "MissingScopeReceipt",
    )?;
    let mut next = Shadow::default();
    for scope in selected {
        let receipt = by_scope[scope];
        if receipt.status != Completion::Complete {
            require(matches!(policy, Policy::AllowPartial), "IncompleteScope")?;
            next.partial = true;
            next.gaps.insert(scope.clone());
            continue;
        }
        let facts = match &receipt.output {
            Output::Full(facts) => facts.clone(),
            Output::Delta {
                base: expected,
                upserts,
                deletes,
            } => {
                require(base.scope_root(scope)? == *expected, "DeltaBaseMismatch")?;
                require(
                    upserts.keys().all(|key| !deletes.contains(key)),
                    "ConflictingDelta",
                )?;
                let mut facts = base.scopes[scope].clone();
                for key in deletes {
                    require(facts.remove(key).is_some(), "DeleteUnknownShard")?;
                }
                facts.extend(upserts.clone());
                facts
            }
            Output::Reuse {
                base: expected,
                input_witness,
            } => {
                require(!invalidated.contains(scope), "InvalidatedScopeCannotReuse")?;
                require(
                    valid_hash(input_witness) && base.scope_root(scope)? == *expected,
                    "InvalidReuseProof",
                )?;
                base.scopes[scope].clone()
            }
        };
        validate(&facts)?;
        next.scopes.insert(scope.clone(), facts);
    }
    Ok(next)
}
