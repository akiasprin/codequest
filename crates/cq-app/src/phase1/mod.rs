mod index;
mod languages;
mod mcp;
mod query;
mod repl;
pub mod service;
use cq_core::{Error, Result};
pub use index::IndexRequest;
use serde_json::{Value, json};
pub use service::Service;
use std::{collections::HashMap, ffi::OsString, path::PathBuf};

pub struct Options {
    pub command: String,
    pub flags: HashMap<String, OsString>,
    pub trailing: Vec<OsString>,
}
impl Options {
    pub fn parse(args: Vec<OsString>) -> Result<Self> {
        let command = args
            .first()
            .and_then(|s| s.to_str())
            .unwrap_or("help")
            .to_owned();
        let mut flags = HashMap::new();
        let mut i = 1;
        let mut trailing = vec![];
        while i < args.len() {
            if args[i] == "--" {
                trailing = args[i + 1..].to_vec();
                break;
            }
            let key = args[i]
                .to_str()
                .ok_or_else(|| Error::new("InvalidArguments", "option name must be UTF-8"))?;
            if !key.starts_with("--") {
                trailing = args[i..].to_vec();
                break;
            }
            if let Some((k, v)) = key.split_once('=') {
                flags.insert(k.into(), v.into());
                i += 1;
                continue;
            }
            if [
                "--no-docs",
                "--allow-partial",
                "--regex",
                "--include-candidates",
                "--include-external",
                "--auto-refresh",
                "--watch",
                "--help",
            ]
            .contains(&key)
            {
                flags.insert(key.into(), "true".into());
                i += 1;
                continue;
            }
            let value = args.get(i + 1).ok_or_else(|| {
                Error::new("InvalidArguments", format!("missing value for {key}"))
            })?;
            if flags.insert(key.into(), value.clone()).is_some() {
                return Err(Error::new("InvalidArguments", format!("duplicate {key}")));
            }
            i += 2;
        }
        Ok(Self {
            command,
            flags,
            trailing,
        })
    }
    pub fn text(&self, key: &str) -> Option<String> {
        self.flags
            .get(key)
            .map(|s| s.to_string_lossy().into_owned())
    }
    pub fn path(&self, key: &str) -> Option<PathBuf> {
        self.flags.get(key).map(PathBuf::from)
    }
    pub fn yes(&self, key: &str) -> bool {
        self.text(key).is_some_and(|s| s == "true")
    }
    pub fn store(&self) -> PathBuf {
        self.path("--store")
            .or_else(|| std::env::var_os("CQ_STORE").map(PathBuf::from))
            .unwrap_or_else(|| {
                std::env::var_os("XDG_CACHE_HOME")
                    .map(PathBuf::from)
                    .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")))
                    .unwrap_or_else(std::env::temp_dir)
                    .join("codequest")
            })
    }
}
pub fn run(args: Vec<OsString>) -> Result<(Value, i32)> {
    let options = Options::parse(args)?;
    let service = std::sync::Arc::new(Service::open(&options.store())?);
    match options.command.as_str() {
        "export-snapshot" => {
            let project = options
                .text("--project")
                .ok_or_else(|| Error::new("MissingProject", "export needs --project"))?;
            let output = options
                .path("--output")
                .ok_or_else(|| Error::new("MissingOutput", "export needs --output"))?;
            let snapshot = service
                .store
                .load_semantics(&project, options.text("--revision").as_deref())?;
            let mut file = std::fs::File::create(&output)?;
            serde_json::to_writer(&mut file, &snapshot)?;
            file.sync_all()?;
            Ok((
                json!({"state":"exported","revision":snapshot.revision,"path":output,"execution_path":"explicit_full_export_job"}),
                0,
            ))
        }
        "resolve-store" => {
            let project = options
                .text("--project")
                .ok_or_else(|| Error::new("MissingProject", "resolve-store needs --project"))?;
            let revision = options
                .text("--revision")
                .ok_or_else(|| Error::new("MissingRevision", "resolve-store needs --revision"))?;
            Ok((
                serde_json::to_value(service.store.resolve_publication(&project, &revision)?)?,
                0,
            ))
        }
        "rebuild-store" | "compact-store" | "retention-preview" | "retention-apply" => {
            let project = options
                .text("--project")
                .ok_or_else(|| Error::new("MissingProject", "maintenance needs --project"))?;
            let op = match options.command.as_str() {
                "rebuild-store" => "rebuild",
                "compact-store" => "compact",
                "retention-preview" => "retention_preview",
                _ => "retention_apply",
            };
            let mut request = json!({"op":op,"project":project});
            if op == "retention_apply" {
                let path = options.path("--request").ok_or_else(|| {
                    Error::new(
                        "MissingRequest",
                        "retention-apply needs the reviewed plan --request FILE",
                    )
                })?;
                let value: serde_json::Value = serde_json::from_reader(std::fs::File::open(path)?)?;
                if let Some(id) = value["plan_file"].as_str() {
                    if value["project"] != project {
                        return Err(Error::new("WrongProject", "retention plan"));
                    }
                    request["plan_file"] = json!(id);
                } else {
                    request["plan"] = value;
                }
            }
            Ok((
                cq_store::runtime::Client::new(&service.store.root)?.call(request)?,
                0,
            ))
        }
        "migrate-store" => {
            let project = options
                .text("--project")
                .ok_or_else(|| Error::new("MissingProject", "migrate-store needs --project"))?;
            Ok((service.store.migrate_paged(&project)?, 0))
        }
        "help" | "--help" => Ok((
            json!({"commands":["capture run / inspect","index --root DIR [--language rust|go|c|cpp|auto] [--name NAME] [--compdb FILE] [-- cargo check ...]","query --project NAME --json '{\"operation\":\"search\",\"text\":\"JSON\"}'","repl [--project NAME]","mcp [--auto-refresh]","projects","status --project NAME","refresh --project NAME","compare --project NAME --base REV --target REV","annotate --project NAME --json ANNOTATION","annotations --project NAME","cypher --project NAME --text 'MATCH (n) RETURN count(*)'","docs --project NAME","rustdoc --project NAME","schema --project NAME","migrate-store --project NAME","rebuild-store --project NAME","compact-store --project NAME","retention-preview --project NAME","retention-apply --project NAME --request PLAN.json"],"store":service.store.root}),
            0,
        )),
        "publish-snapshot" => {
            let path = options.path("--snapshot").ok_or_else(|| {
                Error::new(
                    "MissingSnapshot",
                    "provide --snapshot for recovery of a verified unpublished snapshot",
                )
            })?;
            let snapshot: cq_core::model::SemanticSnapshot =
                serde_json::from_slice(&std::fs::read(path)?)?;
            let expected = snapshot.previous.clone();
            service
                .store
                .publish_paged_semantics(&snapshot, expected.as_deref())?;
            Ok((service::index_summary(&snapshot), 0))
        }
        "index" => {
            let request = IndexRequest::from_options(&options)?;
            Ok((service.index(request)?, 0))
        }
        "refresh" => {
            let project = options
                .text("--project")
                .ok_or_else(|| Error::new("MissingProject", "refresh needs --project"))?;
            Ok((service.refresh(&project)?, 0))
        }
        "projects" => Ok((json!(service.store.list_projects()?), 0)),
        "repl" => {
            repl::run(service, &options)?;
            Ok((json!({"state":"repl_closed"}), 0))
        }
        "mcp" => {
            mcp::run(service, options.yes("--auto-refresh"))?;
            Ok((Value::Null, 0))
        }
        "next"
        | "build_configuration"
        | "members"
        | "relations"
        | "bodies"
        | "capabilities"
        | "occurrences"
        | "query"
        | "search"
        | "trace"
        | "path"
        | "source"
        | "outline"
        | "details"
        | "definition"
        | "type_definition"
        | "references"
        | "callers"
        | "callees"
        | "implementations"
        | "overview"
        | "topology"
        | "entrypoints"
        | "schema"
        | "coverage"
        | "gaps"
        | "contexts"
        | "hotspots"
        | "cycles"
        | "clusters"
        | "tour"
        | "semantic"
        | "docs"
        | "rustdoc"
        | "document"
        | "doc_search"
        | "search_code"
        | "at"
        | "impact"
        | "status"
        | "compare"
        | "changes"
        | "annotate"
        | "annotations"
        | "cypher"
        | "delete"
        | "traces" => {
            let project = options.text("--project").ok_or_else(|| {
                Error::new(
                    "MissingProject",
                    "use --project or select a project in REPL",
                )
            })?;
            let args = query::arguments(&options)?;
            Ok((
                service.dispatch(
                    &project,
                    &options.command,
                    args,
                    options.text("--revision").as_deref(),
                )?,
                0,
            ))
        }
        other => Err(Error::new("UnknownCommand", other)),
    }
}

mod checkpoint;
