//! CodeQuest's occurrence-preserving export, before graph edge aggregation.
use ide::{
    AnalysisHost, FileId, FilePosition, FileRange, GotoDefinitionConfig, StaticIndex,
    VendoredLibrariesConfig,
};
use ide_db::ra_fixture::RaFixtureConfig;
use load_cargo::{LoadCargoConfig, ProcMacroServerChoice, load_workspace_at};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::PathBuf};
use syntax::ast::{HasArgList, HasAttrs, HasDocComments, HasName};
use syntax::{AstNode, AstToken, TextRange, ast};

fn span(r: FileRange) -> Value {
    json!({"file":r.file_id.index(),"start":u32::from(r.range.start()),"end":u32::from(r.range.end())})
}
fn range(file: FileId, r: TextRange) -> Value {
    span(FileRange {
        file_id: file,
        range: r,
    })
}
pub fn run() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    let root = PathBuf::from(
        args.get(2)
            .ok_or_else(|| anyhow::anyhow!("expected project directory"))?,
    )
    .canonicalize()?;
    let output = PathBuf::from(
        args.get(3)
            .ok_or_else(|| anyhow::anyhow!("expected output file"))?,
    );
    // rust-project.json is the owning input. Do not run Cargo to reconstruct a
    // different crate graph or silently enable all-targets/test cfg.
    anyhow::ensure!(
        root.join("rust-project.json").is_file(),
        "explicit rust-project.json required"
    );
    let load = LoadCargoConfig {
        load_out_dirs_from_check: false,
        with_proc_macro_server: ProcMacroServerChoice::Sysroot,
        prefill_caches: false,
        num_worker_threads: 4,
        proc_macro_processes: 1,
    };
    let (db, vfs, _) = load_workspace_at(&root, &Default::default(), &load, &|_| {})?;
    let host = AnalysisHost::with_database(db);
    emit(
        &root,
        &output,
        &host,
        &vfs,
        json!({"database_reused":false,"changed_files":0,"workspace_reloaded":true}),
        None,
    )
}

fn emit(
    root: &std::path::Path,
    output: &std::path::Path,
    host: &AnalysisHost,
    vfs: &vfs::Vfs,
    incremental: Value,
    progress: Option<&Value>,
) -> anyhow::Result<()> {
    use std::collections::{BTreeMap, HashMap};
    use std::io::Write;
    let analysis = host.analysis();
    let workspace = vfs::VfsPath::from(vfs::AbsPathBuf::assert_utf8(root.to_path_buf()));
    let files = StaticIndex::graph_source_files(
        &analysis,
        VendoredLibrariesConfig::Included {
            workspace_root: &workspace,
        },
    );
    let input: Value = serde_json::from_slice(&std::fs::read(root.join("input-map.json"))?)?;
    let owners: HashMap<_, _> = input
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            Some((
                v["path"].as_str()?.to_owned(),
                v["unit"].as_str()?.to_owned(),
            ))
        })
        .collect();
    let mut groups = BTreeMap::<String, Vec<FileId>>::new();
    for file in files {
        let path = vfs.file_path(file).to_string();
        groups
            .entry(
                owners
                    .get(&path)
                    .cloned()
                    .unwrap_or_else(|| "native-source-root".into()),
            )
            .or_default()
            .push(file);
    }
    let dir = output.with_extension("hir-shards");
    std::fs::create_dir_all(&dir)?;
    let mut jobs = vec![];
    for (unit, files) in groups {
        for chunk in files.chunks(32) {
            jobs.push((unit.clone(), chunk.to_vec()));
        }
    }
    let configured = std::env::var("CQ_HIR_WORKERS")
        .ok()
        .map(|s| s.parse::<usize>())
        .transpose()?;
    let workers = configured.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(48)
    });
    anyhow::ensure!((1..=48).contains(&workers), "CQ_HIR_WORKERS must be 1..48");
    let workers = workers.min(jobs.len().max(1));
    let report = |phase: &str, done: usize, total: usize, shard: Value| -> anyhow::Result<()> {
        if let Some(id) = progress {
            println!(
                "{}",
                json!({"protocol":"codequest-session","version":1,"id":id,"state":"progress","phase":phase,"completed":done,"total":total,"workers":if phase=="crate_export"{workers}else{1},"shard":shard})
            );
            std::io::stdout().flush()?;
        }
        Ok(())
    };
    report("crate_export", 0, jobs.len(), Value::Null)?;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let completed = std::sync::Mutex::new(0usize);
    let batches = std::thread::scope(|scope| -> anyhow::Result<Vec<Value>> {
        let mut handles = vec![];
        for _ in 0..workers {
            // Analysis snapshots are Send and share one immutable Salsa
            // revision. No worker mutates the AnalysisHost or its crate graph.
            let snapshot = host.analysis();
            let jobs = &jobs;
            let dir = &dir;
            let next = &next;
            let completed = &completed;
            let report = &report;
            let incremental = &incremental;
            handles.push(scope.spawn(move||->anyhow::Result<Vec<Value>>{
                let mut rows=vec![];
                loop {
                    let index=next.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
                    let Some((unit,files))=jobs.get(index)else{break;};
                    let path=dir.join(format!("{index:06}.json"));let started=std::time::Instant::now();
                    emit_part(root,&path,&snapshot,vfs,incremental.clone(),files)?;
                    let item=json!({"unit":unit,"shard":index,"source_files":files.len(),"elapsed_ms":started.elapsed().as_millis()as u64,"bytes":std::fs::metadata(&path)?.len(),"path":path});
                    {let mut done=completed.lock().unwrap();*done+=1;report("crate_export",*done,jobs.len(),item.clone())?;}
                    rows.push(item);
                }
                Ok(rows)
            }));
        }
        let mut rows = vec![];
        for handle in handles {
            rows.extend(
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("HIR shard worker panicked"))??,
            );
        }
        Ok(rows)
    })?;
    let mut timings = batches;
    timings.sort_by_key(|v| v["shard"].as_u64().unwrap());
    let parts = timings
        .iter()
        .map(|v| PathBuf::from(v["path"].as_str().unwrap()))
        .collect::<Vec<_>>();
    report("merge", 0, parts.len() * 2, Value::Null)?;
    // Token IDs are local to a shard. Reconcile exact native key + declaration
    // coordinates before concatenation, keeping ambiguous declarations distinct.
    let mut tokens = HashMap::<String, u64>::new();
    let mut symbols = Vec::<Value>::new();
    let mut remaps = vec![];
    let mut call_bases = vec![];
    let mut calls = 0u64;
    for (part_index, part) in parts.iter().enumerate() {
        let data: Value =
            serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(part)?))?;
        let mut map = HashMap::new();
        for symbol in data["symbols"].as_array().unwrap() {
            let key = serde_json::to_string(&(
                &symbol["key"],
                &symbol["definition"],
                &symbol["body"],
                &symbol["kind"],
            ))?;
            let token = *tokens.entry(key).or_insert_with(|| {
                let id = symbols.len() as u64;
                let mut row = symbol.clone();
                row["token"] = json!(id);
                symbols.push(row);
                id
            });
            map.insert(symbol["token"].as_u64().unwrap(), token);
            if symbols[token as usize]["external"] == true && symbol["external"] == false {
                let mut row = symbol.clone();
                row["token"] = json!(token);
                symbols[token as usize] = row;
            }
        }
        remaps.push(map);
        call_bases.push(calls);
        calls += data["calls"].as_array().unwrap().len() as u64;
        report("merge", part_index + 1, parts.len() * 2, Value::Null)?;
    }
    let mut writer = std::io::BufWriter::new(std::fs::File::create(output)?);
    let header = json!({"protocol":"codequest-hir-export","schema_version":1,"base_commit":"2850ecba80311bebd4cdaa9fedc5321533b5b1e7","patch_digest":option_env!("CQ_PRODUCER_PATCH_SHA"),"incremental":incremental,"shards":timings});
    writer.write_all(b"{")?;
    let mut separator = false;
    for (k, v) in header.as_object().unwrap() {
        if separator {
            writer.write_all(b",")?
        }
        separator = true;
        serde_json::to_writer(&mut writer, k)?;
        writer.write_all(b":")?;
        serde_json::to_writer(&mut writer, v)?;
    }
    writer.write_all(b",\"symbols\":")?;
    serde_json::to_writer(&mut writer, &symbols)?;
    drop(symbols);
    let fields = [
        "references",
        "parents",
        "relations",
        "bodies",
        "calls",
        "doc_links",
        "files",
        "comments",
        "parameters",
        "diagnostics",
        "scoped_documentation",
        "nominal_members",
        "lexical_parents",
    ];
    let mut streams = fields
        .iter()
        .map(|field| {
            let path = dir.join(format!("merge-{field}.json"));
            Ok((
                std::io::BufWriter::new(std::fs::File::create(&path)?),
                path,
                false,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut seen_files = BTreeSet::new();
    // Read each shard once for all arrays. Re-parsing the entire export for
    // every field multiplied JSON decoding cost by the number of fields.
    for (i, part) in parts.iter().enumerate() {
        let mut data: Value =
            serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(part)?))?;
        for (field, (stream, _, comma)) in fields.iter().zip(&mut streams) {
            for mut row in data[*field]
                .as_array_mut()
                .map(std::mem::take)
                .unwrap_or_default()
            {
                if *field == "files" && !seen_files.insert(row["id"].as_u64().unwrap()) {
                    continue;
                }
                let fields: &[&str] = match *field {
                    "references" => &["target"],
                    "parents" => &["child", "parent_token"],
                    "nominal_members" => &["child", "owner"],
                    _ => &[],
                };
                for key in fields {
                    if let Some(old) = row[*key].as_u64() {
                        row[*key] = json!(
                            remaps[i]
                                .get(&old)
                                .ok_or_else(|| anyhow::anyhow!("unbound shard token"))?
                        );
                    }
                }
                if *field == "references"
                    && let Some(index) = row["call_index"].as_u64()
                {
                    row["call_index"] = json!(index + call_bases[i]);
                }
                if *comma {
                    stream.write_all(b",")?
                }
                *comma = true;
                serde_json::to_writer(&mut *stream, &row)?;
            }
        }
        report("merge", parts.len() + i + 1, parts.len() * 2, Value::Null)?;
    }
    report("merge_write", 0, fields.len(), Value::Null)?;
    for (index, (field, (mut stream, path, _))) in fields.iter().zip(streams).enumerate() {
        stream.flush()?;
        drop(stream);
        writer.write_all(b",")?;
        serde_json::to_writer(&mut writer, field)?;
        writer.write_all(b":[")?;
        std::io::copy(&mut std::fs::File::open(&path)?, &mut writer)?;
        writer.write_all(b"]")?;
        std::fs::remove_file(path)?;
        report("merge_write", index + 1, fields.len(), Value::Null)?;
    }
    writer.write_all(b"}")?;
    writer.flush()?;
    std::fs::write(
        dir.join("timings.json"),
        serde_json::to_vec_pretty(&timings)?,
    )?;
    Ok(())
}

fn emit_part(
    root: &std::path::Path,
    output: &std::path::Path,
    analysis: &ide::Analysis,
    vfs: &vfs::Vfs,
    incremental: Value,
    files: &[FileId],
) -> anyhow::Result<()> {
    let _ = root;
    let index = StaticIndex::compute_graph_files(analysis, files);
    let kind_tags: std::collections::HashMap<_, _> =
        index.codequest_definition_kinds().into_iter().collect();
    let mut file_ids = BTreeSet::new();
    let mut symbols = vec![];
    let mut references = vec![];
    for (id, t) in index.tokens.iter_ref() {
        if let Some(r) = t.definition {
            file_ids.insert(r.file_id);
        }
        if let Some(r) = t.definition_body {
            file_ids.insert(r.file_id);
        }
        let documentation_source = t.definition_body.or(t.definition).map(span);
        let mut types = vec![];
        if !t.external {
            if let Some(def) = t.definition {
                if let Some(result) = analysis.goto_type_definition(FilePosition {
                    file_id: def.file_id,
                    offset: def.range.start(),
                })? {
                    for nav in result.info {
                        file_ids.insert(nav.file_id);
                        types.push(json!({"name":nav.name.as_str(),"source":range(nav.file_id,nav.focus_range.unwrap_or(nav.full_range)),"body":range(nav.file_id,nav.full_range)}));
                    }
                }
            }
        }
        symbols.push(json!({"token":id.raw(),"definition_kind":kind_tags.get(&id.raw()),"key":t.stable_id,"name":t.display_name,"qualified_name":t.qualified_name,"kind":format!("{:?}",t.kind),"signature":t.signature,"documentation":t.documentation.as_ref().map(|d|d.as_str()),"definition":t.definition.map(span),"body":t.definition_body.map(span),"external":t.external,"exported":t.exported,"local":t.local,"trait_member":t.trait_member,"const_function":t.const_function,"documentation_source":documentation_source,"type_definitions":types}));
        for reference in &t.references {
            file_ids.insert(reference.range.file_id);
            references.push(json!({"target":id.raw(),"source":span(reference.range),"is_definition":reference.is_definition,"kind":format!("{:?}",reference.role),"spelling":reference.interface_spelling}));
        }
    }
    let mut parents = vec![];
    for (child, key, name, kind, source, parent_token) in index.codequest_parents() {
        if let Some(r) = source {
            file_ids.insert(r.file_id);
        }
        parents.push(json!({"child":child,"parent_token":parent_token,"key":key,"name":name,"kind":format!("{kind:?}"),"source":source.map(span)}));
    }
    let mut relations = vec![];
    for r in &index.relations {
        if let Some(file) = r.to_definition_file {
            file_ids.insert(file);
        }
        relations.push(json!({"from":r.from,"to":r.to,"from_definition_kind":r.from_definition_kind,"to_definition_kind":r.to_definition_kind,"to_source":r.to_definition_range.map(|(file,start,end)|json!({"file":file.index(),"start":start,"end":end})),"file":r.file_id.index(),"kind":format!("{:?}",r.kind),"to_name":r.to_display_name,"to_qn":r.to_qualified_name,"to_file":r.to_definition_file.map(|f|f.index()),"to_kind":format!("{:?}",r.to_kind),"to_signature":r.to_signature}));
    }
    let mut inner_docs = std::collections::BTreeMap::<(u32, u32), Value>::new();
    let nominal_members = index
        .codequest_nominal_members()
        .into_iter()
        .map(|(child, owner)| json!({"child":child,"owner":owner}))
        .collect::<Vec<_>>();
    let mut lexical_parents = vec![];
    let mut direct_docs = std::collections::BTreeMap::new();
    let mut bodies = vec![];
    let mut calls = vec![];
    let mut doc_links = vec![];
    let mut comments = vec![];
    let mut parameters = vec![];
    let mut diagnostics = vec![];
    for file in &index.files {
        file_ids.insert(file.file_id);
        let (parsed, errors, contexts) = index.codequest_parse(file.file_id);
        for error in errors {
            diagnostics.push(json!({"file":file.file_id.index(),"path":vfs.file_path(file.file_id).to_string(),"message":error}));
        }
        if contexts > 1 {
            diagnostics.push(json!({"file":file.file_id.index(),"path":vfs.file_path(file.file_id).to_string(),"code":"MultipleNativeContexts","contexts":contexts,"message":"file belongs to multiple native modules; retained projection is not a proof of complete per-module coverage"}));
        }
        let syntax = parsed.syntax();
        for node in syntax.descendants() {
            if let Some(owner) = ast::AnyHasDocComments::cast(node.clone()) {
                if let Some(name) = node.children().find_map(ast::Name::cast) {
                    let docs = owner.doc_comments().collect::<Vec<_>>();
                    if let (Some(first), Some(last)) = (docs.first(), docs.last()) {
                        let text = docs
                            .iter()
                            .filter_map(|c| {
                                c.doc_comment()
                                    .map(|(t, _)| t.strip_prefix(' ').unwrap_or(t).to_owned())
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        direct_docs.insert(
                            (
                                file.file_id.index(),
                                u32::from(name.syntax().text_range().start()),
                                u32::from(name.syntax().text_range().end()),
                            ),
                            (
                                text,
                                range(
                                    file.file_id,
                                    TextRange::new(
                                        first.syntax().text_range().start(),
                                        last.syntax().text_range().end(),
                                    ),
                                ),
                            ),
                        );
                    } else if owner
                        .attrs()
                        .any(|a| a.simple_name().is_some_and(|n| n == "doc"))
                    {
                        direct_docs.insert(
                            (
                                file.file_id.index(),
                                u32::from(name.syntax().text_range().start()),
                                u32::from(name.syntax().text_range().end()),
                            ),
                            (String::new(), range(file.file_id, node.text_range())),
                        );
                    }
                }
            }
            let body_kind = if ast::Fn::cast(node.clone()).is_some_and(|f| f.body().is_some()) {
                Some("function")
            } else if ast::ClosureExpr::can_cast(node.kind()) {
                Some("closure")
            } else if let Some(block) = ast::BlockExpr::cast(node.clone()) {
                if block.async_token().is_some() {
                    Some("async_block")
                } else if block.const_token().is_some() {
                    Some("const_block")
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(function) = ast::Fn::cast(node.clone()) {
                if let Some(parent) = node
                    .parent()
                    .into_iter()
                    .flat_map(|p| p.ancestors())
                    .find_map(ast::Fn::cast)
                {
                    if let (Some(child), Some(owner)) = (function.name(), parent.name()) {
                        lexical_parents.push(json!({"child":range(file.file_id,child.syntax().text_range()),"owner":range(file.file_id,owner.syntax().text_range())}));
                    }
                }

                if let (Some(owner), Some(params)) = (function.name(), function.param_list()) {
                    let mut slots = vec![];
                    if let Some(receiver) = params.self_param() {
                        let bindings = receiver
                            .syntax()
                            .descendants_with_tokens()
                            .filter_map(|n| n.into_token())
                            .filter(|t| t.text() == "self")
                            .map(|t| range(file.file_id, t.text_range()))
                            .collect::<Vec<_>>();
                        slots.push(json!({"index":0,"receiver":true,"bindings":bindings}));
                    }
                    let offset = usize::from(params.self_param().is_some());
                    for (i, param) in params.params().enumerate() {
                        let bindings = param
                            .pat()
                            .into_iter()
                            .flat_map(|p| {
                                p.syntax()
                                    .descendants()
                                    .filter_map(ast::IdentPat::cast)
                                    .filter_map(|p| p.name())
                                    .collect::<Vec<_>>()
                            })
                            .map(|name| range(file.file_id, name.syntax().text_range()))
                            .collect::<Vec<_>>();
                        slots.push(json!({"index":i+offset,"receiver":false,"bindings":bindings}));
                    }
                    parameters.push(json!({"owner":range(file.file_id,owner.syntax().text_range()),"slots":slots}));
                }
            }
            if let Some(kind) = body_kind {
                bodies.push(json!({"kind":kind,"source":range(file.file_id,node.text_range()),"block":ast::Fn::cast(node.clone()).and_then(|f|f.body()).map(|b|range(file.file_id,b.syntax().text_range())),"async":ast::Fn::cast(node.clone()).is_some_and(|f|f.async_token().is_some()),"test":ast::Fn::cast(node.clone()).is_some_and(|f|f.attrs().any(|a|a.simple_name().is_some_and(|n|n=="test")))}));
            }
            if let Some(item) = ast::Const::cast(node.clone()) {
                if let (Some(name), Some(body)) = (item.name(), item.body()) {
                    bodies.push(json!({"kind":"const_initializer","source":range(file.file_id,body.syntax().text_range()),"owner":range(file.file_id,name.syntax().text_range())}));
                }
            }
            if let Some(item) = ast::Static::cast(node.clone()) {
                if let (Some(name), Some(body)) = (item.name(), item.body()) {
                    bodies.push(json!({"kind":"static_initializer","source":range(file.file_id,body.syntax().text_range()),"owner":range(file.file_id,name.syntax().text_range())}));
                }
            }
            if let Some(call) = ast::CallExpr::cast(node.clone()) {
                if let Some(callee) = call.expr() {
                    let arguments: Vec<_> = call
                        .arg_list()
                        .into_iter()
                        .flat_map(|a| a.args())
                        .map(|a| range(file.file_id, a.syntax().text_range()))
                        .collect();
                    calls.push(json!({"source":range(file.file_id,node.text_range()),"callee":range(file.file_id,callee.syntax().text_range()),"arguments":arguments,"method":false}));
                }
            }
            if let Some(call) = ast::MethodCallExpr::cast(node.clone()) {
                if let Some(callee) = call.name_ref() {
                    let arguments: Vec<_> = call
                        .arg_list()
                        .into_iter()
                        .flat_map(|a| a.args())
                        .map(|a| range(file.file_id, a.syntax().text_range()))
                        .collect();
                    calls.push(json!({"source":range(file.file_id,node.text_range()),"callee":range(file.file_id,callee.syntax().text_range()),"arguments":arguments,"method":true}));
                }
            }
        }
        // The actual IDE doc resolver is queried at the original comment byte
        // position. A lexical name hit alone never becomes a resolved doc link.
        for token in syntax
            .descendants_with_tokens()
            .filter_map(|t| t.into_token())
        {
            let text = token.text();
            let Some(comment) = ast::Comment::cast(token.clone()) else {
                continue;
            };
            if !comment.is_doc() {
                comments.push(json!({"source":range(file.file_id,token.text_range()),"text":text}));
                continue;
            }
            if comment.is_inner() {
                let module = token
                    .parent()
                    .into_iter()
                    .flat_map(|p| p.ancestors())
                    .find_map(ast::Module::cast);
                let scope = module
                    .as_ref()
                    .map(|m| u32::from(m.syntax().text_range().start()))
                    .unwrap_or(0);
                let owner = module
                    .and_then(|m| m.name())
                    .map(|n| range(file.file_id, n.syntax().text_range()));
                let block=inner_docs.entry((file.file_id.index(),scope)).or_insert_with(||json!({"source":range(file.file_id,token.text_range()),"owner":owner,"text":""}));
                let previous = block["text"].as_str().unwrap_or("");
                let text = comment
                    .doc_comment()
                    .map(|(text, _)| text.strip_prefix(' ').unwrap_or(text))
                    .unwrap_or("");
                block["text"] = json!(if previous.is_empty() {
                    text.to_owned()
                } else {
                    format!("{previous}\n{text}")
                });
                block["source"]["end"] = json!(u32::from(token.text_range().end()));
            }
            let mut offset = 0;
            while let Some(open) = text[offset..].find('[') {
                let start = offset + open + 1;
                let Some(close) = text[start..].find(']') else {
                    break;
                };
                let end = start + close;
                let at = start + usize::from(text[start..end].starts_with('`'));
                offset = end + 1;
                if at >= end {
                    continue;
                }
                let position = FilePosition {
                    file_id: file.file_id,
                    offset: token.text_range().start() + syntax::TextSize::from(at as u32),
                };
                if let Some(result) = analysis.goto_definition(
                    position,
                    &GotoDefinitionConfig {
                        ra_fixture: RaFixtureConfig::default(),
                    },
                )? {
                    let mut targets = vec![];
                    for nav in result.info {
                        file_ids.insert(nav.file_id);
                        targets.push(range(
                            nav.file_id,
                            nav.focus_range.unwrap_or(nav.full_range),
                        ));
                    }
                    if !targets.is_empty() {
                        doc_links.push(json!({"source":range(file.file_id,TextRange::new(position.offset,token.text_range().start()+syntax::TextSize::from(end as u32))),"text":&text[start..end],"targets":targets}));
                    }
                }
            }
        }
    }
    let mut inherited = 0;
    for symbol in &mut symbols {
        let def = &symbol["definition"];
        let key = (
            def["file"].as_u64().unwrap_or(u64::MAX) as u32,
            def["start"].as_u64().unwrap_or(u64::MAX) as u32,
            def["end"].as_u64().unwrap_or(u64::MAX) as u32,
        );
        if let Some((text, source)) = direct_docs.get(&key) {
            if !text.is_empty() {
                symbol["documentation"] = json!(text);
            }
            symbol["documentation_source"] = source.clone();
        } else {
            if symbol["documentation"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
            {
                inherited += 1;
                symbol["unattributed_documentation"] = symbol["documentation"].clone();
            }
            symbol["documentation"] = Value::Null;
            symbol["documentation_source"] = Value::Null;
        }
    }
    if inherited > 0 {
        diagnostics.push(json!({"code":"DocumentationSourceUnavailable","message":format!("{inherited} native documentation values have no direct source owner (including inherited trait docs); raw export retains text, author-source facts exclude it")}));
    }
    let mut files = vec![];
    for id in file_ids {
        files.push(json!({"id":id.index(),"path":vfs.file_path(id).to_string(),"text":analysis.file_text(id)?.as_ref()}));
    }
    let result = json!({"protocol":"codequest-hir-export","schema_version":1,"base_commit":"2850ecba80311bebd4cdaa9fedc5321533b5b1e7","patch_digest":option_env!("CQ_PRODUCER_PATCH_SHA"),"symbols":symbols,"references":references,"parents":parents,"relations":relations,"bodies":bodies,"calls":calls,"doc_links":doc_links,"files":files,"comments":comments,"parameters":parameters,"diagnostics":diagnostics,"scoped_documentation":inner_docs.into_values().collect::<Vec<_>>(),"nominal_members":nominal_members,"lexical_parents":lexical_parents,"incremental":incremental});
    std::fs::write(output, serde_json::to_vec(&result)?)?;
    Ok(())
}

/// A resident Salsa database receives source text changes. A changed crate
/// graph/file set reloads the workspace instead of guessing a partial update.
pub fn serve() -> anyhow::Result<()> {
    use std::io::{BufRead, Write};
    let mut retained = None;
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        let id = request["id"].clone();
        let result = (|| -> anyhow::Result<Value> {
            anyhow::ensure!(
                request["protocol"] == "codequest-session" && request["version"] == 1,
                "unsupported session protocol"
            );
            let root = PathBuf::from(
                request["root"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("root missing"))?,
            )
            .canonicalize()?;
            let output = PathBuf::from(
                request["output"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("output missing"))?,
            );
            let project = std::fs::read(root.join("rust-project.json"))?;
            let inputs: Value =
                serde_json::from_slice(&std::fs::read(root.join("input-map.json"))?)?;
            let mut paths: Vec<_> = inputs
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("input array missing"))?
                .iter()
                .filter_map(|m| m["path"].as_str().map(str::to_owned))
                .collect();
            paths.sort();
            paths.dedup();
            let signature = serde_json::to_vec(&(root.clone(), &project, &paths))?;
            let reused = retained
                .as_ref()
                .is_some_and(|(_, _, _, prior)| prior == &signature);
            if !reused {
                let load = LoadCargoConfig {
                    load_out_dirs_from_check: false,
                    with_proc_macro_server: ProcMacroServerChoice::Sysroot,
                    prefill_caches: false,
                    num_worker_threads: 4,
                    proc_macro_processes: 1,
                };
                let (db, vfs, server) =
                    load_workspace_at(&root, &Default::default(), &load, &|_| {})?;
                retained = Some((AnalysisHost::with_database(db), vfs, server, signature));
            }
            let (host, vfs, _, _) = retained.as_mut().unwrap();
            let mut changed = 0;
            if reused {
                let mut change = hir::ChangeWithProcMacros::default();
                {
                    let analysis = host.analysis();
                    for (file, path) in vfs.iter() {
                        if let Some(path) = path.as_path() {
                            let path: &std::path::Path = path.as_ref();
                            let text = std::fs::read_to_string(path)?;
                            if analysis.file_text(file)?.as_ref() != text {
                                change.change_file(file, Some(text));
                                changed += 1;
                            }
                        }
                    }
                }
                if changed > 0 {
                    host.apply_change(change);
                }
            }
            let stats = json!({"database_reused":reused,"changed_files":changed,"workspace_reloaded":!reused});
            emit(&root, &output, host, vfs, stats.clone(), Some(&id))?;
            Ok(stats)
        })();
        let reply = match result {
            Ok(stats) => {
                json!({"protocol":"codequest-session","version":1,"id":id,"ok":true,"incremental":stats})
            }
            Err(error) => {
                retained = None;
                json!({"protocol":"codequest-session","version":1,"id":id,"ok":false,"error":format!("{error:#}")})
            }
        };
        writeln!(
            std::io::stdout().lock(),
            "{}",
            serde_json::to_string(&reply)?
        )?;
        std::io::stdout().flush()?;
    }
    Ok(())
}
