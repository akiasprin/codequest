//! Isolated libclang worker. USRs and native cursor bindings define relations.
use clang::{Clang, Entity, EntityKind as K, Index};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};
struct Export {
    directory: std::path::PathBuf,
    files: BTreeMap<PathBuf, u64>,
    texts: BTreeMap<PathBuf, String>,
    symbols: Vec<Value>,
    tokens: HashMap<String, usize>,
    references: Vec<Value>,
    parents: Vec<Value>,
    relations: Vec<Value>,
    bodies: Vec<Value>,
    calls: Vec<Value>,
    parameters: Vec<Value>,
    includes: Vec<Value>,
}
impl Export {
    fn path(&self, path: std::path::PathBuf) -> std::path::PathBuf {
        let path = self.directory.join(path);
        path.canonicalize().unwrap_or(path)
    }
    fn span(&mut self, e: Entity<'_>, name: bool) -> Option<Value> {
        let name_ranges = if name { e.get_name_ranges() } else { vec![] };
        let range = if name {
            name_ranges.first().copied().or_else(|| e.get_range())?
        } else {
            e.get_range()?
        };
        let start = if name {
            if name_ranges.is_empty() {
                e.get_location()?.get_spelling_location()
            } else {
                range.get_start().get_spelling_location()
            }
        } else {
            // Declaration prefixes can expand from a different header. A
            // physical spelling start and a call-site end do not form a range.
            range.get_start().get_expansion_location()
        };
        let end = range.get_end().get_expansion_location();
        let file = start.file?;
        if !name && (end.file != Some(file) || end.offset < start.offset) {
            return None;
        }
        let path = self.path(file.get_path());
        let text = file.get_contents()?;
        let id = if let Some(id) = self.files.get(&path) {
            *id
        } else {
            let id = self.files.len() as u64 + 1;
            self.files.insert(path.clone(), id);
            self.texts.insert(path, text);
            id
        };
        let finish = if name && !name_ranges.is_empty() {
            let end = name_ranges.last()?.get_end().get_spelling_location();
            if end.file != Some(file) || end.offset < start.offset {
                return None;
            }
            end.offset
        } else if name {
            // An unavailable native name extent is a point location, never an
            // invented range based on the display name's byte length.
            start.offset
        } else {
            end.offset
        };
        Some(json!({"file":id,"start":start.offset,"end":finish}))
    }
    fn native_range(
        &mut self,
        range: clang::source::SourceRange<'_>,
        spelling: bool,
    ) -> Option<Value> {
        let a = if spelling {
            range.get_start().get_spelling_location()
        } else {
            range.get_start().get_expansion_location()
        };
        let b = if spelling {
            range.get_end().get_spelling_location()
        } else {
            range.get_end().get_expansion_location()
        };
        let file = a.file?;
        if b.file != Some(file) || a.offset > b.offset {
            return None;
        }
        let path = self.path(file.get_path());
        let text = file.get_contents()?;
        let id = if let Some(id) = self.files.get(&path) {
            *id
        } else {
            let id = self.files.len() as u64 + 1;
            self.files.insert(path.clone(), id);
            self.texts.insert(path, text);
            id
        };
        Some(json!({"file":id,"start":a.offset,"end":b.offset}))
    }
    fn token_location(&mut self, e: Entity<'_>, spelling: bool) -> Option<Value> {
        let location = e.get_location()?;
        let at = if spelling {
            location.get_spelling_location()
        } else {
            location.get_expansion_location()
        };
        let file = at.file?;
        let text = file.get_contents()?;
        if at.offset as usize >= text.len() {
            return None;
        }
        let range = clang::source::SourceRange::new(
            file.get_offset_location(at.offset),
            file.get_offset_location(at.offset + 1),
        );
        let token = range.tokenize().into_iter().next()?;
        self.native_range(token.get_range(), true)
    }
    fn reference_locations(&mut self, e: Entity<'_>) -> Value {
        let spelling = self.token_location(e, true);
        let expansion = self.token_location(e, false);
        json!({"spelling":spelling,"expansion":expansion})
    }
    fn ensure(&mut self, e: Entity<'_>) -> usize {
        let declaration = e.get_canonical_entity();
        let definition = e
            .get_definition()
            .or_else(|| e.is_definition().then_some(e));
        let e = definition.unwrap_or(e);
        let location = self.span(e, true);
        let key = e
            .get_usr()
            .map(|u| u.0)
            .unwrap_or_else(|| format!("{:?}@{location:?}", e.get_kind()));
        if let Some(id) = self.tokens.get(&key) {
            return *id;
        }
        let id = self.symbols.len();
        self.tokens.insert(key.clone(), id);
        let kind = match e.get_kind() {
            K::FunctionDecl | K::FunctionTemplate => "Function",
            K::Method | K::Constructor | K::Destructor | K::ConversionFunction => "Method",
            K::StructDecl => "Struct",
            K::ClassDecl | K::ClassTemplate => "Class",
            K::EnumDecl => "Enum",
            K::EnumConstantDecl => "Variant",
            K::UnionDecl => "Union",
            K::Namespace => "Module",
            K::TypedefDecl | K::TypeAliasDecl => "TypeAlias",
            K::FieldDecl => "Field",
            K::ParmDecl => "Parameter",
            K::VarDecl => "Variable",
            K::MacroDefinition => "Macro",
            _ => "Symbol",
        };
        let name = e.get_name().unwrap_or_default();
        let mut qn = vec![name.clone()];
        let mut parent = e.get_semantic_parent();
        while let Some(p) = parent {
            if p.get_kind() == K::TranslationUnit || p.get_semantic_parent().is_none() {
                break;
            }
            if let Some(n) = p.get_name() {
                qn.push(n);
            }
            parent = p.get_semantic_parent();
        }
        qn.reverse();
        let body = self.span(e, false);
        let declaration = declaration
            .get_range()
            .and_then(|r| self.native_range(r, false));
        let definition = definition
            .and_then(|d| d.get_range())
            .and_then(|r| self.native_range(r, false));
        let locations = json!({"declaration":declaration,"definition":definition,"spelling":self.token_location(e,true),"expansion":self.token_location(e,false)});
        let documentation_source = e.get_comment_range().and_then(|r| {
            let a = r.get_start().get_spelling_location();
            let b = r.get_end().get_spelling_location();
            let path = a.file.map(|f| self.path(f.get_path()));
            path.and_then(|p| self.files.get(&p))
                .map(|id| json!({"file":id,"start":a.offset,"end":b.offset}))
        });
        self.symbols.push(json!({"token":id,"key":key,"name":name,"qualified_name":qn.join("::"),"kind":kind,"signature":e.get_type().map(|t|t.get_display_name()).unwrap_or_default(),"definition":location,"locations":locations,"body":body,"external":location.is_none(),"local":e.get_semantic_parent().is_some_and(|p|matches!(p.get_kind(),K::FunctionDecl|K::Method|K::Constructor|K::LambdaExpr)),"exported":e.get_linkage().is_some_and(|l|matches!(l,clang::Linkage::External|clang::Linkage::UniqueExternal)),"trait_member":e.is_virtual_method(),"is_definition":e.is_definition(),"documentation":e.get_comment(),"documentation_source":documentation_source,"type_definitions":[]}));
        if matches!(
            e.get_kind(),
            K::FunctionDecl | K::FunctionTemplate | K::Method | K::Constructor | K::Destructor
        ) {
            let slots=e.get_arguments().unwrap_or_default().into_iter().enumerate().map(|(index,p)|json!({"index":index,"receiver":false,"bindings":self.span(p,true).into_iter().collect::<Vec<_>>()})).collect::<Vec<_>>();
            self.parameters
                .push(json!({"owner":location,"slots":slots}));
        }
        if let Some(p) = e.get_semantic_parent()
            && p.get_kind() != K::TranslationUnit
            && p.get_semantic_parent().is_some()
        {
            let owner = self.ensure(p);
            self.parents.push(json!({"child":id,"key":self.symbols[owner]["key"],"name":self.symbols[owner]["name"],"kind":self.symbols[owner]["kind"],"source":self.symbols[owner]["definition"]}));
        }
        if let Some(t) = e
            .get_type()
            .and_then(|t| t.get_pointee_type().unwrap_or(t).get_declaration())
            && t != e
        {
            let target = self.ensure(t);
            self.symbols[id]["type_definitions"] =
                json!([{"source":self.symbols[target]["definition"]}]);
        }
        if let Some(methods) = e.get_overridden_methods() {
            for target in methods {
                let to = self.ensure(target);
                self.relations.push(json!({"file":self.symbols[id]["definition"]["file"],"to_file":self.symbols[to]["definition"]["file"],"from":self.symbols[id]["key"],"to":self.symbols[to]["key"],"kind":"Overrides"}));
            }
        }
        id
    }
    fn visit(&mut self, e: Entity<'_>, owner: Option<usize>) {
        let kind = e.get_kind();
        if kind == K::InclusionDirective
            && let Some(file) = e.get_file()
        {
            let target = self.path(file.get_path());
            let name = e.get_name();
            let source = self.span(e, false);
            if let Some(text) = file.get_contents() {
                self.native_range(
                    clang::source::SourceRange::new(
                        file.get_offset_location(0),
                        file.get_offset_location(text.len() as u32),
                    ),
                    true,
                );
            }
            self.includes
                .push(json!({"name":name,"target":target,"source":source}));
        }
        let declared = matches!(
            kind,
            K::FunctionDecl
                | K::FunctionTemplate
                | K::Method
                | K::Constructor
                | K::Destructor
                | K::ConversionFunction
                | K::StructDecl
                | K::ClassDecl
                | K::ClassTemplate
                | K::UnionDecl
                | K::EnumDecl
                | K::EnumConstantDecl
                | K::Namespace
                | K::TypedefDecl
                | K::TypeAliasDecl
                | K::FieldDecl
                | K::ParmDecl
                | K::VarDecl
                | K::MacroDefinition
        );
        let next_owner = if declared {
            Some(self.ensure(e))
        } else {
            owner
        };
        if matches!(
            kind,
            K::FunctionDecl | K::Method | K::Constructor | K::Destructor | K::FunctionTemplate
        ) && e.is_definition()
            && let Some(source) = self.span(e, false)
        {
            let block = e
                .get_children()
                .into_iter()
                .find(|c| c.get_kind() == K::CompoundStmt)
                .and_then(|c| self.span(c, false));
            self.bodies.push(json!({"kind":"function","source":source,"block":block,"generated":block.is_none()}));
        }
        if kind == K::LambdaExpr
            && let Some(source) = self.span(e, false)
        {
            self.bodies.push(json!({"kind":"closure","source":source}));
        }
        if kind == K::CallExpr {
            let target = e.get_reference();
            let source = e
                .get_children()
                .first()
                .and_then(|c| self.span(*c, false))
                .or_else(|| self.span(e, false));
            let args = e
                .get_arguments()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|a| self.span(a, false))
                .collect::<Vec<_>>();
            let full_source = self.span(e, false);
            let call_index = self.calls.len();
            self.calls
                .push(json!({"callee":source,"source":full_source,"arguments":args}));
            if let Some(target) = target {
                let id = self.ensure(target);
                let locations = self.reference_locations(e);
                self.references.push(json!({"target":id,"source":source,"locations":locations,"kind":"Call","is_definition":false,"call_index":call_index}));
            }
        } else if matches!(
            kind,
            K::DeclRefExpr
                | K::MemberRefExpr
                | K::TypeRef
                | K::TemplateRef
                | K::NamespaceRef
                | K::MacroExpansion
                | K::BaseSpecifier
        ) && let Some(target) = e.get_reference()
        {
            let id = self.ensure(target);
            if let Some(source) = self.span(e, false) {
                let role = if matches!(kind, K::TypeRef | K::TemplateRef | K::BaseSpecifier) {
                    "Type"
                } else if kind == K::MemberRefExpr {
                    "Access"
                } else {
                    "Reference"
                };
                let locations = self.reference_locations(e);
                self.references.push(json!({"target":id,"source":source,"locations":locations,"kind":role,"is_definition":false}));
            }
            if kind == K::BaseSpecifier
                && let Some(from) = owner
            {
                self.relations.push(json!({"file":self.symbols[from]["definition"]["file"],"to_file":self.symbols[id]["definition"]["file"],"from":self.symbols[from]["key"],"to":self.symbols[id]["key"],"kind":"Extends"}));
            }
        }
        for child in e.get_children() {
            self.visit(child, next_owner);
        }
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    let input: Value =
        serde_json::from_slice(&std::fs::read(args.get(1).ok_or("input required")?)?)?;
    let file = Path::new(input["file"].as_str().ok_or("file required")?);
    let language = input["effective_language"]
        .as_str()
        .filter(|v| matches!(*v, "C" | "Cpp"))
        .ok_or("effective_language required")?;
    let flags: Vec<String> = serde_json::from_value(input["analysis_arguments"].clone())?;
    let directory = PathBuf::from(
        input["directory"]
            .as_str()
            .ok_or("absolute directory required")?,
    )
    .canonicalize()?;
    let expected = if language == "C" { "c" } else { "c++" };
    if !flags.windows(2).any(|p| p[0] == "-x" && p[1] == expected) {
        return Err("analysis arguments do not attest effective language".into());
    }
    let clang = Clang::new().map_err(|e| format!("libclang unavailable: {e}"))?;
    let index = Index::new(&clang, false, false);
    let tu = index
        .parser(file)
        .arguments(&flags)
        .detailed_preprocessing_record(true)
        .parse()
        .map_err(|e| format!("parse: {e:?}"))?;
    let diagnostics: Vec<_> = tu.get_diagnostics().iter().map(|d|json!({"severity":format!("{:?}",d.get_severity()).to_lowercase(),"message":d.to_string()})).collect();
    let mut export = Export {
        directory,
        files: BTreeMap::new(),
        texts: BTreeMap::new(),
        symbols: vec![],
        tokens: HashMap::new(),
        references: vec![],
        parents: vec![],
        relations: vec![],
        bodies: vec![],
        calls: vec![],
        parameters: vec![],
        includes: vec![],
    };
    export.visit(tu.get_entity(), None);
    let mut comments = vec![];
    for (path, id) in &export.files {
        if let Some(file) = tu.get_file(path) {
            let len = export.texts[path].len();
            if len <= u32::MAX as usize {
                let range = clang::source::SourceRange::new(
                    file.get_offset_location(0),
                    file.get_offset_location(len as u32),
                );
                for token in range.tokenize() {
                    if token.get_kind() == clang::token::TokenKind::Comment {
                        let r = token.get_range();
                        comments.push(json!({"source":{"file":id,"start":r.get_start().get_spelling_location().offset,"end":r.get_end().get_spelling_location().offset},"text":token.get_spelling()}));
                    }
                }
            }
        }
    }
    let files: Vec<_> = export
        .files
        .iter()
        .map(|(p, id)| json!({"id":id,"path":p,"text":export.texts[p]}))
        .collect();
    let output = json!({"protocol":"codequest-hir-export","schema_version":1,"unit":input["unit"],"version":clang::get_version(),"symbols":export.symbols,"references":export.references,"parents":export.parents,"relations":export.relations,"bodies":export.bodies,"calls":export.calls,"files":files,"doc_links":[],"diagnostics":diagnostics,"comments":comments,"parameters":export.parameters,"effective_language":language,"effective_analysis_arguments":flags,"includes":export.includes,"include_queries":export.texts.values().any(|t|t.contains("__has_include"))});
    std::fs::write(
        args.get(2).ok_or("output required")?,
        serde_json::to_vec(&output)?,
    )?;
    Ok(())
}
fn main() {
    if std::env::args_os().nth(1).is_some_and(|a| a == "--version") {
        let _library = Clang::new().expect("load libclang for version");
        println!("{}", clang::get_version());
        return;
    }
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(2)
    }
}
