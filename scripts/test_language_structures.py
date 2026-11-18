#!/usr/bin/env python3
"""Strict language-structure regressions through real compilers and public MCP."""
import argparse
import json
from pathlib import Path
import shutil
import sys
import tempfile
import unittest

from test_support.native import EvidenceResult, NativeProject, QueryError, compdb

ROOT = Path(__file__).resolve().parent.parent
PARSER = argparse.ArgumentParser(description=__doc__)
PARSER.add_argument("--cq", type=Path, default=ROOT / "target/release/cq")
PARSER.add_argument("--output", type=Path, required=True)
PARSER.add_argument("--only",choices=["rust","go","cpp"],action="append")
ARGS = PARSER.parse_args()
ARGS.output.mkdir(parents=True, exist_ok=True)


class NativeFixture(unittest.TestCase):
    fixture = ""
    language = ""

    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="cq-structure-")
        cls.addClassCleanup(cls.temp.cleanup)
        directory = Path(cls.temp.name)
        cls.source = directory / "source"
        shutil.copytree(ROOT / "fixtures" / cls.fixture, cls.source)
        options = {"language": cls.language}
        if cls.language == "cpp":
            options["compdb"] = compdb(cls.source, directory,
                ["worker.cpp", "main.cpp", "other.cpp", "plain.c"])
        cls.project = NativeProject(ARGS.cq, cls.source, ARGS.output / cls.__name__, cls.__name__, **options)
        cls.addClassCleanup(cls.project.close)

    def assert_call_to(self, caller, target, *, present=True):
        calls = self.project.calls(caller)
        matches = [r for r in calls if r["entity"]["id"] == target["id"]]
        self.assertEqual(bool(matches), present, [(r["entity"]["qualified_name"], r["relation"]["kind"]) for r in calls])
        return matches


class RustShardBoundaries(unittest.TestCase):
    """More than one export shard, with native identities shared across crates."""
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="cq-rust-shards-")
        cls.addClassCleanup(cls.temp.cleanup)
        root = Path(cls.temp.name)
        (root / "api/src").mkdir(parents=True)
        (root / "app/src").mkdir(parents=True)
        (root / "Cargo.toml").write_text('[workspace]\nmembers=["api","app"]\nresolver="2"\n')
        (root / "api/Cargo.toml").write_text('[package]\nname="shard-api"\nversion="0.1.0"\nedition="2021"\n')
        (root / "api/src/lib.rs").write_text('pub trait Execute { fn execute(&self)->u32; }\npub fn terminal()->u32 { 42 }\n')
        (root / "app/Cargo.toml").write_text('[package]\nname="shard-app"\nversion="0.1.0"\nedition="2021"\n[dependencies]\nshard-api={path="../api"}\n')
        declarations = ''.join(f'pub mod m{i:02};\n' for i in range(40))
        (root / "app/src/lib.rs").write_text(declarations+'pub fn entry()->u32 { m00::stage_00() + m00::Worker.step() }\n')
        for i in range(40):
            target = f'crate::m{i+1:02}::stage_{i+1:02}()' if i < 39 else 'shard_api::terminal()'
            text = f'pub fn stage_{i:02}()->u32 {{ {target} }}\n'
            if i == 0:
                text += 'pub struct Worker;\n'
            if i == 35:
                text += 'impl crate::m00::Worker { pub fn step(&self)->u32 { shard_api::terminal() } }\nimpl shard_api::Execute for crate::m00::Worker { fn execute(&self)->u32 { self.step() } }\n'
            (root / f'app/src/m{i:02}.rs').write_text(text)
        cls.project = NativeProject(ARGS.cq, root, ARGS.output / "rust-shards", "rust-shards",
            language="rust", build_args=["check", "--workspace", "--offline"])
        cls.addClassCleanup(cls.project.close)

    def test_all_forty_cross_file_calls_and_cross_crate_terminal_resolve(self):
        target = self.project.one("terminal", kind="Function")
        for i in reversed(range(40)):
            caller = self.project.one(f"stage_{i:02}", kind="Function")
            self.assertIn(target["id"], {r["entity"]["id"] for r in self.project.calls(caller)}, i)
            target = caller

    def test_nominal_owner_and_trait_impl_cross_shards(self):
        worker = self.project.one("Worker", kind="Struct")
        members = self.project.query("members", subject=worker["id"])
        self.assertTrue({"step", "execute"}.issubset({r["name"] for r in members}), members)
        trait = self.project.one("Execute", kind="Trait")
        self.assertIn(worker["id"], {r["entity"]["id"] for r in self.project.query("implementations", subject=trait["id"])})

    def test_raw_export_remaps_tokens_and_preserves_all_source_shards(self):
        raw = self.project.native_exports()[0]
        self.assertGreaterEqual(len(raw["shards"]), 3)
        self.assertTrue(all(s["source_files"] <= 32 for s in raw["shards"]))
        symbols = {s["token"]: s for s in raw["symbols"]}
        self.assertTrue(raw["references"])
        self.assertTrue(all(r["target"] in symbols for r in raw["references"]))
        self.assertEqual(sum(s["name"] == "terminal" for s in symbols.values()), 1)


class RustStructures(NativeFixture):
    fixture, language = "structures-rust", "rust"

    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        cls.alternate=NativeProject(ARGS.cq,cls.source,ARGS.output/"rust-alternate","rust-alternate",
            language="rust",build_args=["check","--features","alternate","--offline"])
        cls.addClassCleanup(cls.alternate.close)

    def test_same_source_has_different_structural_results_under_cfg(self):
        self.assertFalse(self.project.find("alternate_only"))
        self.assertTrue(self.alternate.find("alternate_only"))
        self.assertFalse(self.alternate.find("default_only"))
        first=self.project.one("run",kind="Function")
        second=self.alternate.one("run",kind="Function")
        self.assertEqual(first["source"]["file_revision"],second["source"]["file_revision"])
        self.assertNotEqual(first["id"],second["id"])

    def test_native_nominal_projection_exists_before_normalization(self):
        raw=self.project.native_exports()[0]
        symbols={s["token"]:s for s in raw["symbols"]}
        links=raw["nominal_members"]
        self.assertTrue(any(symbols[r["child"]]["name"]=="reset" and symbols[r["owner"]]["name"]=="Controller" for r in links))
        self.assertTrue(any(s["name"]=="new" and s["kind"]=="StaticMethod" for s in symbols.values()))

    def test_same_names_are_distinct_and_ambiguous(self):
        rows = self.project.find("Controller", kind="Struct")
        self.assertEqual(len(rows), 2)
        self.assertEqual(len({e["id"] for e in rows}), 2)
        with self.assertRaisesRegex(QueryError, "AmbiguousSymbol"):
            self.project.query("details", subject="Controller")

    def test_alias_and_receivers_bind_to_correct_declarations(self):
        run = self.project.one("run", file="src/lib.rs", kind="Function")
        left = self.project.one("dispatch", file="src/left.rs", kind="Function")
        right = self.project.one("dispatch", file="src/right.rs", kind="Function")
        self.assert_call_to(run, left)
        self.assert_call_to(run, right, present=False)
        handles = [r["entity"] for r in self.project.calls(run) if r["entity"]["name"] == "handle"]
        self.assertEqual({Path(e["file"]).name for e in handles}, {"left.rs", "right.rs"})
        self.assertFalse(any("::Handler::" in e["qualified_name"] for e in handles))

    def test_nominal_members_span_multiple_impl_blocks(self):
        controller = self.project.one("Controller", file="src/left.rs", kind="Struct")
        members = self.project.query("members", subject=controller["id"])
        names = {e["name"] for e in members}
        self.assertTrue({"value", "new", "handle", "reset"}.issubset(names), names)
        self.assertFalse(any(e.get("file", "").endswith("right.rs") for e in members))
        new=next(e for e in members if e["name"]=="new")
        self.assertNotEqual(new["declaring_scope"],controller["id"])
        self.assertTrue(any(r["kind"]=="HasMember" for r in new["membership"]))

    def test_member_kind_filter_does_not_filter_subject(self):
        controller = self.project.one("Controller", file="src/left.rs", kind="Struct")
        members = self.project.query("members", subject=controller["id"], kind="Method", limit=1)
        self.assertTrue(members)
        self.assertEqual({e["kind"] for e in members}, {"Method"})
        self.assertTrue({"new", "handle", "reset"}.issubset({e["name"] for e in members}))

    def test_trait_trace_continues_to_native_override_body_as_candidate(self):
        function=self.project.one("through_trait",kind="Function")
        rows=self.project.query("trace",subject=function["id"],include_candidates=True,depth=4,limit=1)
        target=self.project.one("handle",file="src/left.rs",kind="Method",qualified="Handler")
        self.assertIn(target["id"],{r["entity"]["id"] for r in rows if r.get("entity")})
        candidate=next(r for r in rows if r.get("entity",{}).get("id")==target["id"])
        self.assertEqual(candidate["via"]["kind"],"MayCall")
        self.assertNotIn(target["id"],{r["entity"]["id"] for r in self.project.query("trace",subject=function["id"]) if r.get("entity")})

    def test_trait_implementation_query_is_not_empty(self):
        handler = self.project.one("Handler", file="src/api.rs", kind="Trait")
        rows = self.project.query("implementations", subject=handler["id"])
        self.assertTrue(rows)
        self.assertTrue(any(r["entity"]["name"] == "Controller" for r in rows))

    def test_function_value_is_a_reference_not_a_call(self):
        value = self.project.one("function_value", file="src/lib.rs", kind="Function")
        dispatch = self.project.one("dispatch", file="src/left.rs", kind="Function")
        self.assert_call_to(value, dispatch, present=False)
        refs = self.project.query("occurrences", subject=value["id"])
        self.assertTrue(any(o["declaration"].get("value") == dispatch["id"] and o["kind"] == "References" for o in refs))

    def test_reference_file_scope_uses_occurrence_location(self):
        dispatch = self.project.one("dispatch", file="src/left.rs", kind="Function")
        rows = self.project.query("references", subject=dispatch["id"], file="src/lib.rs")
        self.assertTrue(rows)
        file_ids = {r["source"]["file_revision"] for r in self.project.query("source", file="src/lib.rs")}
        self.assertEqual({r["source"]["file_revision"] for r in rows}, file_ids)

    def test_nested_function_does_not_become_module_sibling(self):
        outer = self.project.one("outer", file="src/lib.rs", kind="Function")
        nested = self.project.one("nested", file="src/lib.rs", kind="Function")
        members = self.project.query("members", subject=outer["id"], kind="Function")
        self.assertIn(nested["id"], {e["id"] for e in members})
        self.assertTrue(any(r["kind"]=="LexicallyContains" for e in members for r in e["membership"]))
        self.assertEqual([r["entity"]["id"] for r in self.project.calls(outer)],[nested["id"]])

    def test_enum_union_alias_and_associated_type_have_real_kinds(self):
        self.project.one("Ready", file="src/lib.rs", kind="Variant")
        self.project.one("Packet", file="src/lib.rs", kind="Union")
        self.project.one("HandlerAlias", file="src/lib.rs", kind="TypeAlias")
        self.assertTrue(self.project.find("Output", file="src/api.rs", kind="Type"))

    def test_cfg_and_string_content_are_not_declarations(self):
        self.assertTrue(self.project.find("default_only", kind="Function"))
        self.assertFalse(self.project.find("alternate_only", kind="Function"))
        self.assertFalse(self.project.find("phantom"))

    def test_deferred_bodies_are_not_immediate_parent_calls(self):
        deferred = self.project.one("deferred", kind="Function")
        self.assertFalse(self.project.calls(deferred))
        bodies = self.project.query("bodies", subject=deferred["id"])
        self.assertTrue({"Closure", "AsyncBlock"}.issubset({b["kind"] for b in bodies}))
        self.assertTrue(all(b["evaluation"] == "Deferred" for b in bodies if b["kind"] in ["Closure", "AsyncBlock"]))


class GoStructures(NativeFixture):
    fixture, language = "structures-go", "go"

    def test_raw_go_facts_keep_alias_and_declaring_receiver(self):
        raw=self.project.native_exports()[0]
        self.assertTrue(any(s["name"]=="Alias" and s["kind"]=="TypeAlias" for s in raw["symbols"]))
        self.assertTrue(any(s["name"]=="Handle" and "Base::" in s["qualified_name"] for s in raw["symbols"]))
        self.assertEqual(sum(s["name"]=="Get" for s in raw["symbols"]),1)

    def test_qualified_method_identity_is_directly_queryable(self):
        method=self.project.one("Handle",file="api/api.go",kind="Method",qualified="Base::")
        definition=self.project.query("definition",subject=method["qualified_name"])
        self.assertEqual([r["id"] for r in definition],[method["id"]])

    def test_cross_package_same_named_functions_are_both_exact(self):
        run = self.project.one("Run", file="worker/worker.go", kind="Function")
        local = self.project.one("Dispatch", file="worker/worker.go", kind="Function")
        remote = self.project.one("Dispatch", file="api/api.go", kind="Function")
        self.assertNotEqual(local["id"], remote["id"])
        self.assert_call_to(run, local)
        self.assert_call_to(run, remote)

    def test_promoted_method_binds_to_declaring_embedded_type(self):
        run = self.project.one("Run", file="worker/worker.go", kind="Function")
        base = self.project.one("Handle", file="api/api.go", kind="Method", qualified="Base")
        other = self.project.one("Handle", file="worker/worker.go", kind="Method")
        self.assert_call_to(run, base)
        self.assert_call_to(run, other)

    def test_embedding_proves_cross_package_interface_implementation(self):
        handler = self.project.one("Handler", file="api/api.go", kind="Interface")
        implementations = self.project.query("implementations", subject=handler["id"])
        self.assertTrue({"Base", "Service", "Other"}.issubset({r["entity"]["name"] for r in implementations}))

    def test_same_named_fields_do_not_cross_receiver_types(self):
        for reader, owner in [("ReadLeft", "Left"), ("ReadRight", "Right")]:
            function = self.project.one(reader, kind="Function")
            fields = [r["entity"] for r in self.project.query("relations", subject=function["id"], relation="Accesses")]
            self.assertEqual(len(fields), 1)
            self.assertIn(owner, fields[0]["qualified_name"])

    def test_alias_and_generic_method_retain_structure(self):
        self.project.one("Alias", file="worker/worker.go", kind="TypeAlias")
        get = self.project.one("Get", file="worker/worker.go", kind="Method")
        run = self.project.one("Run", kind="Function")
        self.assert_call_to(run, get)

    def test_method_value_does_not_become_call(self):
        value = self.project.one("MethodValue", kind="Function")
        self.assertFalse(self.project.calls(value))
        self.assertTrue(any(o["kind"] == "References" for o in self.project.query("occurrences", subject=value["id"])))

    def test_interface_call_is_not_a_proven_concrete_call(self):
        function = self.project.one("ThroughInterface", kind="Function")
        self.assertFalse(self.project.calls(function))
        rows = self.project.calls(function, include_candidates=True)
        self.assertTrue(rows)
        self.assertTrue(all(r["relation"]["kind"] == "Dispatches" for r in rows))

    def test_wrapped_and_generic_call_expressions_keep_exact_targets(self):
        function=self.project.one("WrappedCalls",kind="Function")
        calls=self.project.calls(function)
        names={r["entity"]["qualified_name"] for r in calls}
        self.assertEqual(names,{"example.test/structures/worker::Dispatch","example.test/structures/api::Dispatch","example.test/structures/worker::Generic"})
        convert=self.project.one("TypeConversion",kind="Function")
        self.assertFalse(self.project.calls(convert,include_candidates=True))
        self.assertFalse([o for o in self.project.query("occurrences",subject=convert["id"]) if o["kind"]=="Calls"])

    def test_interface_trace_reaches_only_native_method_set_candidates(self):
        function=self.project.one("ThroughInterface",kind="Function")
        rows=self.project.query("trace",subject=function["id"],include_candidates=True,depth=4,limit=1)
        targets=[r["entity"] for r in rows if r.get("entity") and r["entity"]["name"]=="Handle"]
        self.assertTrue(any("Base::Handle" in e["qualified_name"] for e in targets),targets)
        self.assertTrue(any("Other::Handle" in e["qualified_name"] for e in targets),targets)
        self.assertTrue(all(e["name"]=="Handle" for e in targets))
        self.assertFalse(any((r.get("via") or {}).get("kind")=="Calls" and r["entity"]["name"]=="Handle" for r in rows))

    def test_fields_filter_and_native_build_selection(self):
        service = self.project.one("Service", kind="Struct")
        members = self.project.query("members", subject=service["id"], kind="Field", limit=1)
        self.assertEqual({m["name"] for m in members}, {"Base", "Name"})
        self.assertFalse(self.project.find("Excluded"))
        self.assertFalse(self.project.find("Phantom"))


class CppStructures(NativeFixture):
    fixture, language = "structures-cpp", "cpp"

    def test_raw_clang_ranges_do_not_mix_macro_header_with_body_file(self):
        exports=self.project.native_exports()
        raw=next(r for r in exports if any(s["name"]=="macro_export" for s in r["symbols"]))
        symbol=next(s for s in raw["symbols"] if s["name"]=="macro_export")
        self.assertEqual(symbol["definition"]["file"],symbol["body"]["file"])
        self.assertTrue(any(s["name"]=="Packet" and s["kind"]=="Union" for r in exports for s in r["symbols"]))
        self.assertTrue(any(s["name"]=="Ready" and s["kind"]=="Variant" for r in exports for s in r["symbols"]))

    def test_out_of_line_overloads_have_distinct_native_identities(self):
        methods = self.project.find("handle", file="worker.cpp", kind="Method")
        self.assertEqual(len(methods), 2)
        self.assertEqual(len({m["id"] for m in methods}), 2)
        self.assertEqual(len({m["signature"] for m in methods}), 2)

    def test_operators_and_constructors_keep_their_class_owner(self):
        operator = self.project.one("operator==", file="worker.cpp", kind="Method")
        constructor = self.project.one("Worker", file="worker.cpp", kind="Method")
        members = self.project.query("members", subject=self.project.one("Worker", file="types.hpp", kind="Class", unit=operator["unit"])["id"])
        self.assertTrue({operator["id"], constructor["id"]}.issubset({m["id"] for m in members}))

    def test_overload_calls_do_not_cross_to_wrong_namespace(self):
        run = self.project.one("run", file="main.cpp", kind="Function")
        calls = self.project.calls(run, include_candidates=True)
        handles = [r for r in calls if r["entity"]["name"] == "handle"]
        self.assertEqual(len(handles), 2)
        self.assertEqual(len({r["entity"]["signature"] for r in handles}), 2)
        dispatches = [r for r in calls if r["entity"]["name"] == "dispatch"]
        self.assertEqual(len(dispatches), 1)
        self.assertEqual(dispatches[0]["entity"]["qualified_name"], "storage::detail::dispatch")

    def test_static_callees_remain_translation_unit_local(self):
        run = self.project.one("run", file="main.cpp", kind="Function")
        other = self.project.one("other_run", file="other.cpp", kind="Function")
        left = [r["entity"] for r in self.project.calls(run) if r["entity"]["name"] == "local_helper"]
        right = [r["entity"] for r in self.project.calls(other) if r["entity"]["name"] == "local_helper"]
        self.assertEqual(len(left), 1)
        self.assertEqual(len(right), 1)
        self.assertNotEqual(left[0]["id"], right[0]["id"])
        self.assertTrue(left[0]["file"].endswith("main.cpp"))
        self.assertTrue(right[0]["file"].endswith("other.cpp"))

    def test_cross_translation_unit_definition_navigation(self):
        run = self.project.one("run", file="main.cpp", kind="Function")
        target = next(r["entity"] for r in self.project.calls(run) if r["entity"]["name"] == "out_of_line")
        definitions = self.project.query("definition", subject=target["id"])
        entities = [r.get("entity", r) for r in definitions]
        self.assertTrue(any(e["file"].endswith("worker.cpp") for e in entities))

    def test_enum_variants_and_c_and_cpp_unions_are_queryable(self):
        self.assertTrue(self.project.find("Ready", kind="Variant"))
        self.assertTrue(self.project.find("Packet", kind="Union"))
        self.project.one("Payload", file="plain.c", kind="Union")
        self.project.one("Busy", file="plain.c", kind="Variant")

    def test_virtual_call_is_a_declaration_not_concrete_execution(self):
        function = self.project.one("dynamic_call", kind="Function")
        self.assertFalse(self.project.calls(function))
        calls = self.project.calls(function, include_candidates=True)
        self.assertTrue(any(r["relation"]["kind"] == "Dispatches" and "Base::handle" in r["entity"]["qualified_name"] for r in calls))

    def test_virtual_trace_reaches_cross_unit_override_without_claiming_direct_execution(self):
        function=self.project.one("dynamic_call",kind="Function")
        rows=self.project.query("trace",subject=function["id"],include_candidates=True,depth=4,limit=1)
        implementations=[r for r in rows if r.get("entity") and r["entity"]["name"]=="handle" and r["entity"]["file"].endswith("worker.cpp")]
        self.assertTrue(implementations)
        self.assertTrue(all(r["via"]["kind"]=="MayCall" for r in implementations))
        page=self.project.page("callees",subject=function["id"])
        self.assertFalse(page["items"])
        self.assertTrue(any(h["category"]=="immediate_dispatch_or_implementation_candidates" and h["count"]>0 for h in page["hidden"]))

    def test_function_values_and_fake_text_are_not_calls(self):
        function = self.project.one("method_value", kind="Function")
        self.assertFalse(self.project.calls(function))
        self.assertFalse(self.project.find("phantom"))
        self.assertFalse(self.project.find("excluded"))

    def test_macro_declaration_prefix_does_not_erase_function_body(self):
        function = self.project.one("macro_export", file="plain.c", kind="Function")
        calls = self.project.calls(function)
        self.assertEqual([r["entity"]["name"] for r in calls], ["local_helper"])
        self.assertTrue(all(r["entity"]["file"].endswith("plain.c") for r in calls))
        bodies = self.project.query("bodies", subject=function["id"])
        self.assertEqual(len(bodies), 1)
        self.assertEqual(bodies[0]["source"]["file_revision"], function["source"]["file_revision"])

    def test_multiple_calls_in_one_macro_expansion_keep_separate_arguments(self):
        function=self.project.one("macro_many_calls",file="plain.c",kind="Function")
        calls=self.project.calls(function)
        self.assertEqual({r["entity"]["name"] for r in calls},{"two","four"})
        occurrences=[o for o in self.project.query("occurrences",subject=function["id"]) if o["kind"]=="Calls"]
        self.assertEqual(len({o["id"] for o in occurrences}),2)
        self.assertEqual(sorted(len(o["arguments"]) for o in occurrences),[2,4])

    def test_outline_kind_filter_is_structural(self):
        run = self.project.one("run", file="main.cpp", kind="Function")
        outline = self.project.query("outline", file="types.hpp", kind="Class", context=run["unit"])
        self.assertTrue(outline)
        self.assertTrue(all(e["kind"] == "Class" for e in outline))
        self.assertNotIn("value", {e["name"] for e in outline})


class UpstreamGoWorkspace(NativeFixture):
    fixture,language="upstream/compiler-graph-go-work","go"

    def test_workspace_modules_and_cross_module_named_signatures(self):
        service=self.project.one("Service",kind="Struct")
        for name in ["Greeter","Transformer"]:
            interface=self.project.one(name,kind="Interface")
            self.assertIn(service["id"],{r["entity"]["id"] for r in self.project.query("implementations",subject=interface["id"])})

    def test_blank_assertions_are_absent_and_init_functions_remain_distinct(self):
        self.assertFalse(self.project.find("_"))
        initializers=self.project.find("init",kind="Function")
        self.assertEqual(len(initializers),2)
        self.assertEqual(len({e["id"] for e in initializers}),2)
        self.assertEqual(len({e["provider_id"] for e in __import__("json").loads(self.project.snapshot_path.read_text())["entities"] if e["name"]=="init"}),2)

    def test_same_named_calls_and_fields_stay_in_their_modules(self):
        run=self.project.one("Run",kind="Function")
        local=self.project.one("Resolve",qualified="example.com/impl::",kind="Function")
        remote=self.project.one("Resolve",qualified="example.com/api::",kind="Function")
        self.assert_call_to(run,local)
        self.assert_call_to(run,remote,present=False)
        reader=self.project.one("ReadLeft",kind="Function")
        fields=self.project.query("relations",subject=reader["id"],relation="Accesses")
        self.assertEqual([r["entity"]["qualified_name"] for r in fields],["example.com/api::Left::Value"])

    def test_build_tags_and_dynamic_assertion_calls_remain_explicit(self):
        self.assertFalse(self.project.find("GhostFromExcludedBuildTag"))
        run=self.project.one("Run",kind="Function")
        occurrences=self.project.query("occurrences",subject=run["id"])
        self.assertTrue(any(o["kind"]=="Calls" and o["executable"]["state"]=="Unknown" for o in occurrences))
        self.assertFalse(any(r["entity"]["name"] in ["Len","Greet"] for r in self.project.calls(run)))

    def test_imported_testdata_is_captured_because_it_actually_compiled(self):
        function=self.project.one("ReadProto",kind="Function")
        target=self.project.one("Describe",file="testdata/proto/proto.go",kind="Function")
        self.assert_call_to(function,target)
        self.assertFalse(target["external"])

    def test_uncompiled_test_variants_are_disclosed_as_a_gap(self):
        self.assertFalse(self.project.find("TestRun"))
        coverage=self.project.query("coverage")
        self.assertTrue(any(c.get("capability")=="go_test_variants" and c["state"]=="Partial" for c in coverage))


if __name__ == "__main__":
    suite = unittest.TestSuite(unittest.defaultTestLoader.loadTestsFromTestCase(c)
        for language,c in [("rust",RustStructures),("rust",RustShardBoundaries),("go",GoStructures),("cpp",CppStructures),("go",UpstreamGoWorkspace)]
        if not ARGS.only or language in ARGS.only)
    result = unittest.TextTestRunner(verbosity=2, resultclass=EvidenceResult).run(suite)
    (ARGS.output / "results.json").write_text(json.dumps({"tests": result.testsRun,
        "passed": result.wasSuccessful(), "cases": result.cases}, indent=2))
    sys.exit(not result.wasSuccessful())
