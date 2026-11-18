#!/usr/bin/env python3
"""Build a pinned external HIR producer; only its task-owned checkout is patched."""
import argparse,hashlib,json,os,shutil,subprocess
from pathlib import Path
parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--fetch',action='store_true',help='fetch locked provider dependencies before the offline build');args=parser.parse_args()
root=Path(__file__).resolve().parent.parent
lock=json.loads((root/'providers/rust-hir/producer.lock.json').read_text())
tools=Path(os.environ.get('CQ_TOOLS_DIR',root/'.codequest-tools'))/'rust-hir'
source=tools/'source'; build=Path(os.environ.get('CQ_HIR_BUILD_DIR',tools/'build'))
if not source.exists():
 subprocess.run(['git','clone','--no-checkout',lock['repository'],str(source)],check=True)
 subprocess.run(['git','checkout','--detach',lock['commit']],cwd=source,check=True)
assert subprocess.check_output(['git','-C',str(source),'rev-parse','HEAD'],text=True).strip()==lock['commit'],'producer checkout must match lock'
overlay=(root/'providers/rust-hir/src/codequest_export.rs').read_bytes()
(source/'crates/rust-analyzer/src/codequest_export.rs').write_bytes(overlay)
lib=source/'crates/rust-analyzer/src/lib.rs';s=lib.read_text()
if 'pub mod codequest_export;' not in s:s+='\npub mod codequest_export;\n'
lib.write_text(s)
main=source/'crates/rust-analyzer/src/bin/main.rs';s=subprocess.check_output(['git','show',lock['commit']+':crates/rust-analyzer/src/bin/main.rs'],cwd=source,text=True);needle='fn main() -> anyhow::Result<ExitCode> {'
s=s.replace(needle,needle+'\n    if std::env::args_os().nth(1).is_some_and(|a|a=="codequest-export"){rust_analyzer::codequest_export::run()?;return Ok(ExitCode::SUCCESS); }\n    if std::env::args_os().nth(1).is_some_and(|a|a=="codequest-serve"){rust_analyzer::codequest_export::serve()?;return Ok(ExitCode::SUCCESS); }\n')
main.write_text(s)
index=source/'crates/ide/src/static_index.rs';s=subprocess.check_output(['git','show',lock['commit']+':crates/ide/src/static_index.rs'],cwd=source,text=True)
marker="impl<'a> StaticIndex<'a> {"
if 'pub fn codequest_parents' not in s:
 s=s.replace(marker,marker+'''
    pub fn codequest_nominal_members(&self)->Vec<(usize,usize)>{
        hir::attach_db(self.db,||self.def_map.iter().filter_map(|(definition,child)|{
            if !matches!(definition,Definition::Function(_)|Definition::Const(_)|Definition::TypeAlias(_)){return None;}
            let Definition::SelfType(implementation)=definition.enclosing_definition(self.db)? else{return None;};
            let nominal=Definition::Adt(implementation.self_ty(self.db).as_adt()?);
            Some((child.raw(),self.def_map.get(&nominal)?.raw()))
        }).collect())
    }
    pub fn codequest_definition_kinds(&self)->Vec<(usize,String)>{self.def_map.iter().map(|(definition,token)|(token.raw(),format!("{:?}",std::mem::discriminant(definition)))).collect()}
    pub fn codequest_parse(&self,file:FileId)->(ast::SourceFile,Vec<String>,usize){
        hir::attach_db(self.db,||{let sema=Semantics::new(self.db);let count=sema.file_to_module_defs(file).count();let file=sema.attach_first_edition(file);let errors=file.parse_errors(self.db).into_iter().flatten().take(128).map(|e|e.to_string()).collect();(sema.parse(file),errors,count)})
    }
    /// Native semantic parents for the occurrence-preserving CodeQuest export.
    pub fn codequest_parents(&self) -> Vec<(usize,String,String,SymbolInformationKind,Option<FileRange>,Option<usize>)> {
        hir::attach_db(self.db, || {
        let sema=Semantics::new(self.db);
        self.def_map.iter().filter_map(|(def,token)| {
            let parent=def.enclosing_definition(self.db)?;
            let krate=def.krate(self.db)?;
            let nav=parent.try_to_nav(&sema).map(UpmappingResult::call_site);
            let source=nav.map(|n|FileRange{file_id:n.file_id,range:n.focus_or_full_range()});
            Some((token.raw(),stable_id_for_definition(self.db,&sema,krate,parent),parent.name(self.db).map(|n|n.display(self.db,krate.edition(self.db)).to_string()).unwrap_or_else(||"<impl>".into()),def_to_kind(self.db,parent),source,self.def_map.get(&parent).map(|token|token.raw())))
        }).collect()
        })
    }
''',1)
s=s.replace('    pub from: String,','    pub from: String,\n    pub from_definition_kind: String,\n    pub to_definition_kind: String,\n    pub to_definition_range: Option<(FileId,u32,u32)>,',1)
s=s.replace('        from: stable_id_for_definition(db, sema, from_crate, from),','        from_definition_kind:format!("{:?}",std::mem::discriminant(&from)),\n        to_definition_kind:format!("{:?}",std::mem::discriminant(&to)),\n        to_definition_range:nav.as_ref().map(|n|{let r=n.focus_or_full_range();(n.file_id,u32::from(r.start()),u32::from(r.end()))}),\n        from: stable_id_for_definition(db, sema, from_crate, from),',1)
index.write_text(s)
patch=subprocess.check_output(['git','-C',str(source),'diff','--','crates/ide/src/static_index.rs','crates/rust-analyzer/src/lib.rs','crates/rust-analyzer/src/bin/main.rs'])
digest=hashlib.sha256(overlay+patch).hexdigest();env=os.environ.copy();env.update(CARGO_TARGET_DIR=str(build),CQ_PRODUCER_PATCH_SHA=digest,CARGO_BUILD_JOBS='4',RUSTUP_TOOLCHAIN=lock['rust_toolchain'])
if args.fetch: subprocess.run(['cargo','fetch','--locked'],cwd=source,env=env,check=True)
subprocess.run(['cargo','build','--release','-p','rust-analyzer','--offline'],cwd=source,env=env,check=True)
binary=tools/'bin/cq-rust-hir';binary.parent.mkdir(parents=True,exist_ok=True)
# Replace the executable atomically: an existing HIR session may still run the
# previous inode while the next session starts the newly built producer.
temporary=binary.with_name(binary.name+'.new');shutil.copy2(build/'release/rust-analyzer',temporary);temporary.replace(binary)
(tools/'built.json').write_text(json.dumps({**lock,'patch_digest':digest,'binary_digest':hashlib.sha256(binary.read_bytes()).hexdigest()},indent=2)+'\n')
print(binary)
