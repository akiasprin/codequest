#!/usr/bin/env python3
"""CKG-derived foundation contracts using actual builds and frozen public queries."""
import argparse,json,shutil,sys,tempfile,unittest,subprocess
from pathlib import Path
from test_support.native import NativeProject,EvidenceResult,compdb
ROOT=Path(__file__).resolve().parent.parent
p=argparse.ArgumentParser(description=__doc__);p.add_argument('--cq',type=Path,default=ROOT/'target/release/cq');p.add_argument('--output',type=Path,required=True);p.add_argument('--only',choices=['go','cpp','identity','checkpoint','resident'],action='append');ARGS=p.parse_args();ARGS.output.mkdir(parents=True,exist_ok=True)
class GoTestVariants(unittest.TestCase):
 @classmethod
 def setUpClass(cls):
  cls.temp=tempfile.TemporaryDirectory(prefix='cq-go-tests-');cls.addClassCleanup(cls.temp.cleanup);cls.root=Path(cls.temp.name)/'source';shutil.copytree(ROOT/'fixtures/upstream/compiler-graph-go-work',cls.root)
  with (cls.root/'impl/impl_test.go').open('a') as f:f.write('\ntype T = testing.T\nfunc TestAlias(t *T){ _=Run() }\nfunc BenchmarkRun(b *testing.B){ _=Run() }\nfunc FuzzRun(f *testing.F){ _=Run() }\nfunc TestMain(m *testing.M){ _=Run() }\n')
  (cls.root/'impl/external_test.go').write_text('package impl_test\nimport("testing"; "example.com/impl")\nfunc TestExternal(t *testing.T){ _ = impl.Run() }\n')
  cls.project=NativeProject(ARGS.cq,cls.root,ARGS.output/'go','go-tests',language='go',build_args=['test','-a','-vet=off','-run=^$','./api/...','./impl/...']);cls.addClassCleanup(cls.project.close)
  cls.snapshot=json.loads(cls.project.snapshot_path.read_text());cls.units={u['id']:u for u in cls.snapshot['units']}
 def test_compiler_distinguishes_production_internal_and_external_variants(self):
  names=[u['name'].get('value','') for u in self.units.values()];self.assertIn('example.com/impl',names);self.assertTrue(any('example.com/impl [' in n for n in names));self.assertTrue(any('example.com/impl_test [' in n for n in names))
  run=self.project.find('Run',kind='Function');self.assertEqual(len(run),2);self.assertEqual(len({r['unit'] for r in run}),2);self.assertEqual(len({r['source']['file_revision'] for r in run}),1)
 def test_job_binding_uses_exact_files_and_does_not_invent_an_unbuilt_production_job(self):
  tests=[u for u in self.units.values() if u['role']=='Test'];self.assertEqual(len(tests),2);self.assertTrue(all(u['binding']=='ObservedInvocation' for u in tests))
  observed=[u for u in self.units.values() if u['binding']=='ObservedInvocation'];self.assertEqual(len({u['invocation']['value'] for u in observed}),len(observed))
  production=next(u for u in self.units.values() if u['name']['value']=='example.com/impl');self.assertEqual(production['binding'],'UnattributedArtifact')
  self.assertTrue(any(g['code']=='GoCompilerInvocationNotBound' and g['scope']=='example.com/impl' for g in self.snapshot['gaps']))
  context=next(c for c in self.snapshot['contexts'] if c['entry_unit']==production['id']);self.assertEqual(context['status'],'Partial')
 def test_runnable_tests_and_examples_get_test_edges(self):
  for name in ['TestRun','TestAlias','BenchmarkRun','FuzzRun','TestMain','ExampleRun','TestExternal']:
   function=self.project.one(name,kind='Function');self.assertTrue(self.project.query('relations',subject=function['id'],relation='Tests'),name)
 def test_lookalike_tests_and_examples_never_get_test_edges(self):
  for name in ['Testhelper','TestMethod','ExampleNotRun']:
   function=self.project.one(name);self.assertFalse(self.project.query('relations',subject=function['id'],relation='Tests'),name)
 def test_external_test_call_binds_to_test_selected_instance(self):
  function=self.project.one('TestExternal');calls=self.project.calls(function);target=next(r['entity'] for r in calls if r['entity']['name']=='Run');self.assertIn(' [',self.units[target['unit']]['name']['value']);self.assertTrue(self.project.query('bodies',subject=target['id']))
 def test_same_bytes_keep_context_scoped_occurrences(self):
  runs=self.project.find('Run',kind='Function');self.assertEqual(len(runs),2)
  for run in runs:
   rows=self.project.query('occurrences',subject=run['id'],context=run['unit']);self.assertTrue(rows);self.assertTrue(all(r['unit']==run['unit'] for r in rows))

class CppLocations(unittest.TestCase):
 @classmethod
 def setUpClass(cls):
  cls.temp=tempfile.TemporaryDirectory(prefix='cq-cpp-locations-');cls.addClassCleanup(cls.temp.cleanup);directory=Path(cls.temp.name);cls.root=directory/'source';cls.root.mkdir()
  (cls.root/'api.h').write_text('#pragma once\nint declared(int value);\n#define CALL_DECLARED(v) declared(v)\n#define NESTED(v) CALL_DECLARED(v)\n#define CALL_ARG(fn,v) fn(v)\nextern int shared;\n#define READ_TWICE (shared + shared)\n')
  (cls.root/'api.cpp').write_text('#include "api.h"\nint declared(int value){return value+1;}\n')
  (cls.root/'main.cpp').write_text('#include "api.h"\nint macro_call(){return NESTED(4); }\nint argument_call(){return CALL_ARG(declared,5); }\nint shared;\nint double_read(){return READ_TWICE;}\n')
  db=compdb(cls.root,directory,['api.cpp','main.cpp']);cls.project=NativeProject(ARGS.cq,cls.root,ARGS.output/'cpp','cpp-locations',language='cpp',compdb=db);cls.addClassCleanup(cls.project.close)
  cls.snapshot=json.loads(cls.project.snapshot_path.read_text());cls.files={f['id']:f for f in cls.snapshot['sources']['files']}
 def text(self,anchor):
  self.assertIsNotNone(anchor);f=self.files[anchor['file_revision']];data=(self.project.store/'blobs'/f['blob']['digest']).read_bytes();return data[anchor['start']:anchor['end']].decode()
 def test_declaration_and_definition_stay_independent_across_files(self):
  definition=self.project.one('declared',file='api.cpp',kind='Function');loc=definition['locations'];self.assertTrue(self.files[loc['declaration']['file_revision']]['logical_path']['display'].endswith('api.h'));self.assertTrue(self.files[loc['definition']['file_revision']]['logical_path']['display'].endswith('api.cpp'));self.assertIn('return value+1',self.text(loc['definition']));self.assertNotIn('return',self.text(loc['declaration']))
 def test_macro_reference_keeps_header_spelling_and_caller_expansion(self):
  caller=self.project.one('macro_call',kind='Function');rows=[r for r in self.project.query('occurrences',subject=caller['id']) if r['kind']=='Calls'];self.assertEqual(len(rows),1);loc=rows[0]['locations'];self.assertEqual(self.text(loc['spelling']),'declared');self.assertEqual(self.text(loc['expansion']),'NESTED');self.assertNotEqual(loc['spelling']['file_revision'],loc['expansion']['file_revision'])
 def test_macro_argument_spelling_does_not_become_macro_definition(self):
  caller=self.project.one('argument_call',kind='Function');call=next(r for r in self.project.query('occurrences',subject=caller['id']) if r['kind']=='Calls');loc=call['locations'];self.assertEqual(self.text(loc['spelling']),'declared');self.assertEqual(self.text(loc['expansion']),'CALL_ARG');self.assertEqual(loc['spelling']['file_revision'],loc['expansion']['file_revision']);self.assertNotEqual(loc['spelling']['start'],loc['expansion']['start'])

 def test_two_macro_tokens_at_one_expansion_keep_distinct_occurrence_identities(self):
  caller=self.project.one('double_read',kind='Function');shared=self.project.one('shared',kind='Variable',file='main.cpp')
  rows=[r for r in self.project.query('occurrences',subject=caller['id']) if r['declaration']=={'state':'Resolved','value':shared['id']}]
  self.assertEqual(len(rows),2);self.assertEqual(len({r['id'] for r in rows}),2)
  self.assertEqual({self.text(r['locations']['spelling']) for r in rows},{'shared'})
  self.assertEqual(len({r['locations']['spelling']['start'] for r in rows}),2)
  self.assertEqual(len({r['locations']['expansion']['start'] for r in rows}),1)

class StableHandles(unittest.TestCase):
 def test_go_handle_survives_rebuild_body_edit_and_added_file(self):
  with tempfile.TemporaryDirectory(prefix='cq-handle-') as tmp:
   root=Path(tmp)/'source';root.mkdir();(root/'go.mod').write_text('module example.test/stable\ngo 1.25\n');file=root/'main.go';file.write_text('package stable\nfunc Run() int { return 1 }\n')
   project=NativeProject(ARGS.cq,root,ARGS.output/'identity-go','identity-go',language='go')
   try:
    before=project.one('Run',kind='Function');handle=before.get('stable_handle');self.assertTrue(handle and handle.startswith('cq1:'),before)
    observed_before={r['observed']['id'] for r in project.query('observations')}
    for edit in [False,True]:
     if edit:
      file.write_text('// shifts every source position\npackage stable\nfunc Run() int { return 2 }\n');(root/'aaa.go').write_text('package stable\nfunc Added() {}\n')
     command=[str(ARGS.cq),'index','--root',str(root),'--language','go','--name',project.name,'--store',str(project.store)];r=subprocess.run(command,text=True,capture_output=True,timeout=120);self.assertEqual(r.returncode,0,r.stderr)
     after=project.query('definition',subject=handle);self.assertEqual(len(after),1);self.assertEqual(after[0]['stable_handle'],handle);self.assertEqual(after[0]['id'],before['id']);self.assertEqual(after[0]['name'],'Run')
     observed_after={r['observed']['id'] for r in project.query('observations')};self.assertNotEqual(observed_after,observed_before);observed_before=observed_after
     if edit:self.assertNotEqual(after[0]['source']['file_revision'],before['source']['file_revision'])
    # New process, no in-memory handle registry.
    r=subprocess.run([str(ARGS.cq),'query','--store',str(project.store),'--project',project.name,'--json',json.dumps({'operation':'definition','subject':handle})],text=True,capture_output=True,timeout=30);self.assertEqual(r.returncode,0,r.stderr);self.assertEqual(json.loads(r.stdout)['items'][0]['stable_handle'],handle)
   finally:project.close()

 def test_rust_handle_survives_real_capture_and_hir_replay(self):
  with tempfile.TemporaryDirectory(prefix='cq-rust-handle-') as tmp:
   root=Path(tmp)/'source';(root/'src').mkdir(parents=True);(root/'Cargo.toml').write_text('[package]\nname="stable-fixture"\nversion="0.1.0"\nedition="2024"\n[workspace]\n');source=root/'src/lib.rs';source.write_text('#![allow(dead_code)]\n#[inline] pub fn run() -> usize { 1 }\nfn first(){const VALUE:usize=1; struct Local; let _=VALUE;}\nfn second(){const VALUE:usize=2; struct Local; let _=VALUE;}\n')
   project=NativeProject(ARGS.cq,root,ARGS.output/'identity-rust','identity-rust',language='rust')
   try:
    before=project.one('run',kind='Function');handle=before['stable_handle'];self.assertTrue(handle.startswith('cq1:'))
    local_handles=[]
    for name in ['VALUE','Local']:
     locals=project.find(name);self.assertEqual(len(locals),2);self.assertEqual(len({r['stable_handle'] for r in locals}),2);self.assertTrue(all(r['stable_handle'].startswith('cqg1:') for r in locals))
     for local in locals:self.assertEqual(project.query('definition',subject=local['stable_handle'])[0]['id'],local['id'])
     local_handles.extend(r['stable_handle'] for r in locals)
    source.write_text('// shifts source offsets\npub fn run() -> usize { 2 }\n')
    r=subprocess.run([str(ARGS.cq),'refresh','--store',str(project.store),'--project',project.name],text=True,capture_output=True,timeout=180);self.assertEqual(r.returncode,0,r.stderr)
    after=project.query('definition',subject=handle);self.assertEqual(len(after),1);self.assertEqual(after[0]['stable_handle'],handle);self.assertNotEqual(after[0]['source']['file_revision'],before['source']['file_revision'])
    for local_handle in local_handles:
     missing=subprocess.run([str(ARGS.cq),'query','--store',str(project.store),'--project',project.name,'--json',json.dumps({'operation':'definition','subject':local_handle})],text=True,capture_output=True,timeout=30)
     self.assertNotEqual(missing.returncode,0);self.assertIn('MissingSymbol',missing.stderr)
   finally:project.close()
 def test_cpp_handle_survives_body_edit_and_native_unit_cache_reuse(self):
  with tempfile.TemporaryDirectory(prefix='cq-cpp-handle-') as tmp:
   directory=Path(tmp);root=directory/'source';root.mkdir();a=root/'alpha.cpp';a.write_text('int Alpha(){return 1;}\n');(root/'beta.cpp').write_text('int Beta(){return 2;}\n');db=compdb(root,directory,['alpha.cpp','beta.cpp']);project=NativeProject(ARGS.cq,root,ARGS.output/'identity-cpp','identity-cpp',language='cpp',compdb=db)
   try:
    before=project.one('Alpha',kind='Function');handle=before['stable_handle'];a.write_text('// shifted\nint Alpha(){return 3;}\n')
    r=subprocess.run([str(ARGS.cq),'refresh','--store',str(project.store),'--project',project.name],text=True,capture_output=True,timeout=180);self.assertEqual(r.returncode,0,r.stderr);summary=json.loads(r.stdout);self.assertEqual(summary['native_reused'],1)
    self.assertEqual(project.query('definition',subject=handle)[0]['name'],'Alpha')
    generation=project.rpc('tools/call',{'name':'query_codequest','arguments':{'project':project.name,'operation':'generation'}});self.assertEqual([Path(p).name for p in generation['upserts']],['alpha.cpp'])
   finally:project.close()

class CheckpointLifecycle(unittest.TestCase):
 def test_native_noop_delta_delete_and_corrupt_restart_recovery(self):
  with tempfile.TemporaryDirectory(prefix='cq-checkpoint-') as tmp:
   root=Path(tmp)/'source';root.mkdir();(root/'go.mod').write_text('module example.test/checkpoint\ngo 1.25\n');alpha=root/'alpha.go';beta=root/'beta.go';alpha.write_text('package checkpoint\nfunc Alpha() int { return 1 }\n');beta.write_text('package checkpoint\nfunc Beta() int { return 2 }\n')
   project=NativeProject(ARGS.cq,root,ARGS.output/'checkpoint','checkpoint',language='go')
   def refresh():
    r=subprocess.run([str(ARGS.cq),'refresh','--store',str(project.store),'--project',project.name],text=True,capture_output=True,timeout=180);self.assertEqual(r.returncode,0,r.stderr);return json.loads(r.stdout)
   try:
    initial=project.summary['revision'];first=project.rpc('tools/call',{'name':'query_codequest','arguments':{'project':project.name,'operation':'generation'}});self.assertTrue(first['manifest'])
    noop=refresh();self.assertEqual(noop['revision'],initial);self.assertEqual(noop['update']['mode'],'checkpoint_unchanged');self.assertFalse(noop['update']['new_build_observed'])
    alpha.write_text('package checkpoint\nfunc Alpha() int { return 3 }\n');changed=refresh();self.assertNotEqual(changed['revision'],initial)
    generation=project.rpc('tools/call',{'name':'query_codequest','arguments':{'project':project.name,'operation':'generation'}})
    self.assertEqual([Path(k).name for k in generation['upserts']],['alpha.go']);self.assertEqual(generation['deletes'],[])
    beta.unlink();deleted=refresh();generation=project.rpc('tools/call',{'name':'query_codequest','arguments':{'project':project.name,'operation':'generation'}});self.assertEqual([Path(k).name for k in generation['deletes']],['beta.go']);self.assertFalse(project.find('Beta'))
    path=project.store/'revisions'/deleted['revision']/'checkpoint.json';data=json.loads(path.read_text());data['coordinates']['generation']='0'*64;path.write_text(json.dumps(data));recovered=refresh()
    if data.get('format')==2:
     # The checkpoint is now a derived metadata cache. Repair it from the sealed
     # canonical revision without fabricating a new compiler observation/HEAD.
     self.assertEqual(recovered['revision'],deleted['revision']);self.assertEqual(recovered['update']['mode'],'checkpoint_repaired');self.assertFalse(recovered['update']['new_build_observed'])
    else:self.assertNotEqual(recovered['revision'],deleted['revision'])
    self.assertTrue(project.one('Alpha'));self.assertEqual(refresh()['update']['mode'],'checkpoint_unchanged')
   finally:project.close()

class CppCacheInvalidation(unittest.TestCase):
 def refresh(self,project):
  r=subprocess.run([str(ARGS.cq),'refresh','--store',str(project.store),'--project',project.name],text=True,capture_output=True,timeout=180);self.assertEqual(r.returncode,0,r.stderr);return json.loads(r.stdout)
 def test_empty_symbol_header_changes_and_new_shadow_header_invalidate_native_cache(self):
  with tempfile.TemporaryDirectory(prefix='cq-cpp-headers-') as tmp:
   directory=Path(tmp);root=directory/'source';root.mkdir();early=root/'early';late=root/'late';early.mkdir();(late/'sub').mkdir(parents=True)
   header=late/'sub/config.h';header.write_text('#pragma once\n#define VALUE 1\n');source=root/'main.cpp';source.write_text('#include <sub/config.h>\nint run(){return VALUE;}\n');db=compdb(root,directory,['main.cpp']);entries=json.loads(db.read_text());entries[0]['arguments']+=['-I',str(early),'-I',str(late)];db.write_text(json.dumps(entries));project=NativeProject(ARGS.cq,root,ARGS.output/'cpp-cache-shadow','cpp-cache-shadow',language='cpp',compdb=db)
   try:
    header.write_text('#pragma once\n#define VALUE 2\n');changed=self.refresh(project);self.assertEqual(changed['native_reused'],0)
    (early/'sub').mkdir();shadow=early/'sub/config.h';shadow.write_text('#pragma once\ninline int shadowed(){return 3;}\n#define VALUE shadowed()\n');changed=self.refresh(project);self.assertEqual(changed['native_reused'],0);run=project.one('run',kind='Function');self.assertEqual([r['entity']['name'] for r in project.calls(run)],['shadowed'])
    # Rewriting a selected input forces a fresh build but permits unchanged native replay.
    source.write_text(source.read_text()+'\n');self.assertEqual(self.refresh(project)['native_reused'],0)
   finally:project.close()
 def test_optional_include_never_uses_an_unproved_noop_or_native_cache(self):
  with tempfile.TemporaryDirectory(prefix='cq-cpp-optional-') as tmp:
   directory=Path(tmp);root=directory/'source';root.mkdir();source=root/'main.cpp';source.write_text('#if __has_include("optional.h")\n#include "optional.h"\n#else\nint fallback(){return 0;}\n#endif\n');db=compdb(root,directory,['main.cpp']);project=NativeProject(ARGS.cq,root,ARGS.output/'cpp-cache-optional','cpp-cache-optional',language='cpp',compdb=db)
   try:
    unchanged=self.refresh(project);self.assertNotEqual(unchanged['revision'],project.revision);self.assertEqual(unchanged['native_reused'],0)
    (root/'optional.h').write_text('inline int optional(){return 1;}\n');added=self.refresh(project);self.assertEqual(added['native_reused'],0);self.assertTrue(project.find('optional',kind='Function'));self.assertFalse(project.find('fallback',kind='Function'))
   finally:project.close()

class RustResidentLifecycle(unittest.TestCase):
 def test_hir_database_reuses_body_changes_and_reloads_added_deleted_files(self):
  with tempfile.TemporaryDirectory(prefix='cq-hir-resident-') as tmp:
   root=Path(tmp)/'source';(root/'src').mkdir(parents=True);(root/'Cargo.toml').write_text('[package]\nname="resident-fixture"\nversion="0.1.0"\nedition="2024"\n[workspace]\n');lib=root/'src/lib.rs';helper=root/'src/helper.rs';lib.write_text('pub mod helper;\npub fn run()->usize{helper::value()}\n');helper.write_text('pub fn value()->usize{1}\npub fn other()->usize{9}\n');project=NativeProject(ARGS.cq,root,ARGS.output/'resident-rust','resident-rust',language='rust')
   def refresh():return project.rpc('tools/call',{'name':'refresh_repository','arguments':{'project':project.name}})
   try:
    helper.write_text('pub fn value()->usize{2}\npub fn other()->usize{9}\n');first=refresh();self.assertFalse(first['analysis'][0]['database_reused'])
    helper.write_text('pub fn value()->usize{3}\npub fn other()->usize{9}\n');second=refresh();self.assertTrue(second['analysis'][0]['database_reused']);self.assertGreater(second['analysis'][0]['changed_files'],0);self.assertFalse(second['analysis'][0]['workspace_reloaded'])
    generation=project.rpc('tools/call',{'name':'query_codequest','arguments':{'project':project.name,'operation':'generation'}});self.assertEqual([Path(p).name for p in generation['upserts']],['helper.rs'])
    lib.write_text('pub mod helper;\npub fn run()->usize{helper::other()}\n');redirected=refresh();self.assertTrue(redirected['analysis'][0]['database_reused']);run=project.one('run',kind='Function');self.assertEqual([r['entity']['name'] for r in project.calls(run)],['other'])
    extra=root/'src/extra.rs';extra.write_text('pub fn added(){}\n');lib.write_text('pub mod helper;\npub mod extra;\npub fn run()->usize{helper::value()}\n');added=refresh();self.assertTrue(added['analysis'][0]['workspace_reloaded']);self.assertTrue(project.one('added'))
    extra.unlink();lib.write_text('pub mod helper;\npub fn run()->usize{helper::value()}\n');removed=refresh();self.assertTrue(removed['analysis'][0]['workspace_reloaded']);self.assertFalse(project.find('added'))
   finally:project.close()

if __name__=='__main__':
 classes=[c for name,c in [('go',GoTestVariants),('cpp',CppLocations),('cpp',CppCacheInvalidation),('identity',StableHandles),('checkpoint',CheckpointLifecycle),('resident',RustResidentLifecycle)] if not ARGS.only or name in ARGS.only]
 tests=[test for c in classes for test in unittest.defaultTestLoader.loadTestsFromTestCase(c)]
 pattern=__import__('os').environ.get('CQ_TEST_MATCH')
 if pattern:tests=[test for test in tests if pattern in test.id()]
 result=unittest.TextTestRunner(verbosity=2,resultclass=EvidenceResult).run(unittest.TestSuite(tests))
 (ARGS.output/'results.json').write_text(json.dumps({'tests':result.testsRun,'passed':result.wasSuccessful(),'cases':result.cases},indent=2));sys.exit(not result.wasSuccessful())
