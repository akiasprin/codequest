//! Exact names precede bounded lexical windows. Continuations keep small merge
//! frontiers instead of an ever-growing seen set or a whole-corpus rank pass.
use super::*;
const WINDOW: usize = 32;
#[derive(Default, Serialize, Deserialize)]
pub(super) struct TextPlan {
    #[serde(default)]
    filtered_kinds: BTreeSet<String>,
    version: u8,
    phase: u8,
    streams: Option<Vec<Stream>>,
    window: Vec<Value>,
    window_work: usize,
}
impl TextPlan {
    pub(super) fn finished() -> Self {
        Self {
            version: 1,
            phase: 2,
            ..Default::default()
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Stream {
    group: String,
    after: Option<String>,
    done: bool,
    input: VecDeque<FactRow>,
}
impl Stream {
    fn new(group: String) -> Self {
        Self {
            group,
            after: None,
            done: false,
            input: VecDeque::new(),
        }
    }
}
fn order(row: &FactRow) -> String {
    format!("{}:{}", crate::model::digest(row.key.as_bytes()), row.key)
}
impl Engine<'_> {
    fn text_groups(&self, q: &QueryRequest) -> Result<Vec<String>> {
        let mut terms = search::tokenize(&q.text)
            .into_iter()
            .map(|t| group("search_term", &t))
            .collect::<BTreeSet<_>>();
        // One trigram covers every full-name substring match. Prefer the most
        // selective rather than the lexicographically first gram.
        if self.reader.metadata()["search_index"]["version"] == 1 {
            let mut best = None;
            for gram in search::grams(&q.text) {
                let name = group("search_gram", &gram);
                let count = self.reader.count(&name)?;
                if best.as_ref().is_none_or(|(_, n)| count < *n) {
                    best = Some((name, count));
                }
                if count == 0 {
                    break;
                }
            }
            if let Some((name, _)) = best {
                terms.insert(name);
            }
        } else {
            terms.clear();
        }
        let mut lexical =
            if terms.is_empty() || terms.len() > 16 || search::grams(&q.text).is_empty() {
                vec![
                    if q.include_external {
                        "all:entity"
                    } else {
                        "internal:entity"
                    }
                    .into(),
                ]
            } else {
                terms.into_iter().collect::<Vec<_>>()
            };
        let mut facets = vec![vec![
            if q.include_external {
                "all:entity"
            } else {
                "internal:entity"
            }
            .into(),
        ]];
        if let Some(kind) = &q.kind {
            facets.push(if kind.eq_ignore_ascii_case("Callable") {
                vec![
                    group("kind_entity", "function"),
                    group("kind_entity", "method"),
                ]
            } else {
                vec![group("kind_entity", &kind.to_ascii_lowercase())]
            });
        }
        if let Some(ctx) = self.context(q)? {
            facets.push(ctx.units.iter().map(|u| group("unit", u)).collect());
        }
        if q.file.is_some() || q.directory.is_some() {
            let mut scoped = q.clone();
            scoped.operation = "outline".into();
            facets.push(self.entity_groups(&scoped)?);
        }
        let cost = |groups: &[String]| -> Result<u64> {
            groups
                .iter()
                .try_fold(0u64, |n, g| Ok(n.saturating_add(self.reader.count(g)?)))
        };
        let mut best = cost(&lexical)?;
        for facet in facets {
            let n = cost(&facet)?;
            if n < best {
                best = n;
                lexical = facet;
            }
        }
        lexical.sort();
        lexical.dedup();
        Ok(lexical)
    }
    fn next_text_row(
        &self,
        plan: &mut TextPlan,
        q: &QueryRequest,
        start: Instant,
    ) -> Result<Option<FactRow>> {
        if plan.streams.is_none() {
            let groups = if plan.phase == 0 {
                if self.reader.metadata()["search_index"]["version"] == 1 && !q.text.is_empty() {
                    let exact = group("lookup_folded", &q.text.to_lowercase());
                    let best = self.reader.count(&exact)?;
                    let mut groups = vec![exact];
                    // A scoped exact-name pass must still see excluded kinds
                    // for diagnostics. Compare file/context scopes, not kind.
                    if q.file.is_some() || q.directory.is_some() || q.context.is_some() {
                        let mut scoped = q.clone();
                        scoped.operation = "outline".into();
                        scoped.kind = None;
                        let candidates = self.entity_groups(&scoped)?;
                        let count = candidates.iter().try_fold(0u64, |n, g| {
                            Ok::<_, Error>(n.saturating_add(self.reader.count(g)?))
                        })?;
                        if count < best {
                            groups = candidates;
                        }
                    }
                    groups
                } else if !q.text.is_empty() {
                    self.text_groups(q)?
                } else {
                    vec![]
                }
            } else {
                self.text_groups(q)?
            };
            plan.streams = Some(groups.into_iter().map(Stream::new).collect());
        }
        let streams = plan.streams.as_mut().unwrap();
        for stream in streams.iter_mut() {
            if stream.input.is_empty() && !stream.done {
                if start.elapsed()
                    >= Duration::from_millis(q.timeout_ms.saturating_sub(20.min(q.timeout_ms / 10)))
                {
                    return Ok(None);
                }
                let batch = self.reader.scan(
                    &stream.group,
                    stream.after.as_deref(),
                    32.min(q.max_work).max(1),
                )?;
                stream.after = batch.after;
                stream.done = batch.complete;
                stream.input = batch.rows.into();
            }
        }
        let next = streams
            .iter()
            .filter_map(|s| s.input.front().map(order))
            .min();
        let Some(next) = next else {
            return Ok(None);
        };
        let mut row = None;
        for stream in streams {
            if stream.input.front().is_some_and(|r| order(r) == next) {
                let value = stream.input.pop_front().unwrap();
                if row.is_none() {
                    row = Some(value);
                }
            }
        }
        Ok(row)
    }
    pub(super) fn execute_text(
        &self,
        q: &QueryRequest,
        resume: Option<QueryContinuation>,
        ctx: Option<&AnalysisContext>,
        start: Instant,
        digest: String,
    ) -> Result<QueryResult> {
        let mut state: State = if let Some(c) = resume {
            if c.revision != self.reader.revision() || c.query_digest != digest {
                return Err(Error::new("StaleCursor", "query or revision changed"));
            }
            match c.state {
                ContinuationState::Store { project, state } if project == self.reader.project() => {
                    serde_json::from_value(state)?
                }
                _ => {
                    return Err(Error::new(
                        "InvalidCursor",
                        "text search needs a fixed store cursor",
                    ));
                }
            }
        } else {
            State {
                text: Some(TextPlan {
                    version: 1,
                    ..Default::default()
                }),
                skip: q.offset,
                ranking_done: true,
                ..Default::default()
            }
        };
        let mut plan = state.text.take().ok_or_else(|| {
            Error::new(
                "CursorPlanChanged",
                "text search plan changed; start the query again",
            )
        })?;
        if plan.version != 1 {
            return Err(Error::new(
                "CursorPlanChanged",
                "unsupported text search plan",
            ));
        }
        let mut items = vec![];
        let mut work = 0;
        let mut bytes = 0;
        while items.len() < q.limit {
            if self.control.is_some_and(|c| c.is_cancelled()) {
                return Err(Error::new("Cancelled", "text query cancelled"));
            }
            if let Some(v) = state.pending.pop_front() {
                let e = self
                    .entity(v["id"].as_str().unwrap())?
                    .ok_or_else(|| Error::new("MissingEntity", "ranked text candidate"))?;
                let mut result = self.summary_for(&e, q.compact)?;
                result["score"] = v["score"].clone();
                result["match_tier"] = v["match_tier"].clone();
                if !q.compact {
                    result["entity_ref"] = json!(e.id);
                }
                let size = serde_json::to_vec(&result)?.len();
                if size > q.max_bytes && items.is_empty() {
                    return Err(Error::new(
                        "QueryByteBudget",
                        "one text result exceeds max_bytes",
                    ));
                }
                if bytes + size > q.max_bytes {
                    state.pending.push_front(v);
                    break;
                }
                if state.skip > 0 {
                    state.skip -= 1;
                    continue;
                }
                bytes += size;
                items.push(result);
                continue;
            }
            let budget = work >= q.max_work
                || start.elapsed()
                    >= Duration::from_millis(
                        q.timeout_ms.saturating_sub(20.min(q.timeout_ms / 10)),
                    );
            if plan.window_work >= WINDOW || budget || plan.phase == 2 {
                plan.window_work = 0;
                plan.window.sort_by(rank_order);
                state.pending = std::mem::take(&mut plan.window).into();
                if !state.pending.is_empty() {
                    continue;
                }
                if budget || plan.phase == 2 {
                    break;
                }
            }
            match self.next_text_row(&mut plan, q, start)? {
                Some(row) => {
                    work += 1;
                    plan.window_work += 1;
                    let e: Entity = serde_json::from_value(row.value)?;
                    let exact = e.name.eq_ignore_ascii_case(&q.text)
                        || e.qualified_name.eq_ignore_ascii_case(&q.text);
                    if exact
                        && q.kind.as_ref().is_some_and(|kind| {
                            !super::super::presentation::kind_matches(e.kind, kind)
                        })
                    {
                        let mut scope = q.clone();
                        scope.kind = None;
                        if self.selected(&e, &scope, ctx)? && self.allowed(&e.provenance, q)? {
                            plan.filtered_kinds.insert(e.kind.label());
                        }
                    }
                    if (plan.phase == 0 && !exact)
                        || (plan.phase == 1 && exact && !q.text.is_empty())
                        || !self.selected(&e, q, ctx)?
                        || !self.allowed(&e.provenance, q)?
                        || !disclosure::visible(&e, q)
                    {
                        continue;
                    }
                    let score = self.score(&e, q)?;
                    if score > 0. {
                        plan.window.push(json!({"id":e.id,"score":score,"match_tier":if plan.phase==0{"exact_name"}else{"lexical"}}));
                    }
                }
                None => {
                    let done = plan
                        .streams
                        .as_ref()
                        .is_some_and(|s| s.iter().all(|s| s.done && s.input.is_empty()));
                    if done {
                        plan.phase += 1;
                        plan.streams = None;
                        plan.window_work = 0;
                        plan.window.sort_by(rank_order);
                        state.pending = std::mem::take(&mut plan.window).into();
                    } else {
                        plan.window.sort_by(rank_order);
                        state.pending = std::mem::take(&mut plan.window).into();
                        if state.pending.is_empty() {
                            break;
                        }
                    }
                }
            }
        }
        state.exhausted = plan.phase == 2;
        if !plan.filtered_kinds.is_empty()
            && !state
                .extra_gaps
                .iter()
                .any(|g| g.code == "KindFilterExcludesExactName")
        {
            state.extra_gaps.push(Gap::new("KindFilterExcludesExactName", q.file.as_deref().unwrap_or(&q.text),
                format!("exact-name matches of kind {:?} were excluded by kind={}; omit kind or use Callable for functions and methods",plan.filtered_kinds,q.kind.as_deref().unwrap_or(""))));
        }
        state.text = Some(plan);
        self.finish_page(q, state, items, work, start, digest, false)
    }
}
