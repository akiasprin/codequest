#!/usr/bin/env python3
"""Pinned real repositories: exact structural and negative query oracles."""
import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import unittest

from prepare_test_corpus import prepare, ROOT
from test_support.native import EvidenceResult, NativeProject

PARSER = argparse.ArgumentParser(description=__doc__)
PARSER.add_argument("--cq", type=Path, default=ROOT / "target/release/cq")
PARSER.add_argument("--corpus", type=Path, default=ROOT / ".codequest-tools/test-corpus")
PARSER.add_argument("--output", type=Path, required=True)
PARSER.add_argument("--fetch", action="store_true", help="fetch locked sources and build dependencies")
PARSER.add_argument("--only", choices=["ripgrep", "gin", "leveldb", "cjson"], action="append")
PARSER.add_argument("--repetitions",type=int,default=1)
ARGS = PARSER.parse_args()
ARGS.output.mkdir(parents=True, exist_ok=True)
LOCK = prepare(ARGS.corpus, fetch=ARGS.fetch)


def command(words, cwd, log):
    result = subprocess.run(words, cwd=cwd, capture_output=True, text=True, timeout=600)
    with log.open("a") as stream:
        stream.write(json.dumps({"command": words, "exit": result.returncode,
            "stdout": result.stdout, "stderr": result.stderr}) + "\n")
    if result.returncode:
        raise AssertionError(f"corpus preparation failed: {words}\n{result.stderr}")


class Corpus(unittest.TestCase):
    name = ""

    @classmethod
    def setUpClass(cls):
        spec = next(r for r in LOCK["repositories"] if r["name"] == cls.name)
        cls.source = (ARGS.corpus / cls.name).resolve()
        output = ARGS.output / cls.name
        output.mkdir(exist_ok=True)
        environment = os.environ.copy()
        environment.update(GOPROXY="off", GOSUMDB="off", GOTOOLCHAIN="local")
        if ARGS.fetch:
            if spec["language"] == "go":
                command(["go", "mod", "download"], cls.source, output / "prepare.jsonl")
            if spec["language"] == "rust":
                command(["cargo", "fetch", "--locked"], cls.source, output / "prepare.jsonl")
        options = {"language": spec["language"]}
        if "translation_units" in spec:
            build = ARGS.corpus.parent / "corpus-builds" / cls.name
            command(["cmake", "-S", str(cls.source), "-B", str(build),
                "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON", "-DCMAKE_POLICY_VERSION_MINIMUM=3.5",
                "-DCMAKE_C_COMPILER=clang", "-DCMAKE_CXX_COMPILER=clang++", "-DBUILD_SHARED_LIBS=OFF",
                *spec["cmake_options"]], cls.source, output / "prepare.jsonl")
            commands = json.loads((build / "compile_commands.json").read_text())
            selected = []
            for relative in spec["translation_units"]:
                entry = next((c for c in commands if Path(c["file"]).resolve() == cls.source / relative), None)
                if entry is None:
                    raise AssertionError(f"CMake did not emit selected translation unit {relative}")
                args = entry.get("arguments") or shlex.split(entry["command"])
                if "-o" in args:
                    target = Path(args[args.index("-o") + 1])
                    if not target.is_absolute():
                        target = Path(entry["directory"]) / target
                    target.parent.mkdir(parents=True, exist_ok=True)
                selected.append(entry)
            compdb = output / "selected_compile_commands.json"
            compdb.write_text(json.dumps(selected, indent=2))
            options["compdb"] = compdb.resolve()
        else:
            options["build_args"] = spec["build_args"]
        cls.project = NativeProject(ARGS.cq, cls.source, output, cls.name,
            environment=environment,repetitions=ARGS.repetitions,diagnostic_export=False, **options)
        cls.addClassCleanup(cls.project.close)
        (output / "scope.json").write_text(json.dumps(spec, indent=2))
        if subprocess.check_output(["git", "status", "--porcelain"], cwd=cls.source, text=True):
            raise AssertionError("real corpus source bytes changed during preparation/indexing")


class Ripgrep(Corpus):
    name = "ripgrep"

    def test_json_members_include_all_impl_blocks(self):
        node = self.project.one("JSON", file="crates/printer/src/json.rs", kind="Struct")
        members = self.project.query("members", subject=node["id"], kind="Method", limit=2)
        self.assertTrue({"sink_with_path", "write_message", "has_written", "get_mut", "into_inner"}.issubset({r["name"] for r in members}))
        self.assertFalse(any("standard.rs" in m.get("file", "") for m in members))

    def test_json_wire_variants_are_exact(self):
        message = self.project.one("Message", file="crates/printer/src/jsont.rs", kind="Enum")
        members = self.project.query("members", subject=message["id"], kind="Variant", limit=1)
        self.assertEqual({m["name"] for m in members}, {"Begin", "End", "Match", "Context"})

    def test_trait_override_query_finds_json_implementation(self):
        methods = self.project.find("matched", file="crates/searcher/src/sink.rs", kind="Method", qualified="sink::Sink::matched")
        self.assertEqual(len(methods), 1)
        implementations = self.project.query("implementations", subject=methods[0]["id"])
        self.assertTrue(any("JSONSink" in r["entity"]["qualified_name"] for r in implementations))

    def test_serializer_calls_the_real_encoder_not_same_named_alternatives(self):
        serializer = self.project.one("serialize", file="crates/printer/src/jsont.rs", kind="Method", qualified="Data<")
        callees = self.project.calls(serializer)
        self.assertEqual([r["entity"]["qualified_name"] for r in callees if r["entity"]["name"] == "base64_standard"], ["jsont::base64_standard"])

    def test_structural_outline_does_not_index_uncompiled_cli(self):
        rows = self.project.query("outline", file="crates/core/main.rs", kind="Function")
        self.assertEqual(rows, [])


class Gin(Corpus):
    name = "gin"

    def test_engine_has_native_fields_and_own_methods(self):
        engine = self.project.one("Engine", file="gin.go", kind="Struct")
        fields = self.project.query("members", subject=engine["id"], kind="Field", limit=2)
        self.assertIn("RouterGroup", {f["name"] for f in fields})
        methods = self.project.query("members", subject=engine["id"], kind="Method", limit=2)
        self.assertIn("ServeHTTP", {m["name"] for m in methods})
        self.assertTrue(all("Engine::" in m["qualified_name"] for m in methods))

    def test_same_named_render_methods_keep_concrete_owners(self):
        json_renderer = self.project.one("JSON", file="render/json.go", kind="Struct")
        members = self.project.query("members", subject=json_renderer["id"], kind="Method")
        self.assertTrue({"Render", "WriteContentType"}.issubset({m["name"] for m in members}))
        self.assertFalse(any("XML::" in m["qualified_name"] for m in members))

    def test_render_interface_implementations_use_go_type_rules(self):
        interface = self.project.one("Render", file="render/render.go", kind="Interface")
        implementations = self.project.query("implementations", subject=interface["id"])
        names = {r["entity"]["name"] for r in implementations}
        self.assertTrue({"JSON", "XML", "String"}.issubset(names), names)

    def test_context_next_retains_unresolved_handler_invocation(self):
        function = self.project.one("Next", file="context.go", kind="Method", qualified="Context::")
        occurrences = self.project.query("occurrences", subject=function["id"])
        self.assertTrue(any(o["kind"] == "Calls" and o["executable"]["state"] == "Unknown" for o in occurrences))

    def test_render_qualified_names_remain_unambiguous(self):
        methods = self.project.find("Render", kind="Method")
        json_methods = [m for m in methods if "::JSON::Render" in m["qualified_name"]]
        xml_methods = [m for m in methods if "::XML::Render" in m["qualified_name"]]
        self.assertEqual(len(json_methods), 1)
        self.assertEqual(len(xml_methods), 1)
        self.assertNotEqual(json_methods[0]["id"], xml_methods[0]["id"])


class LevelDB(Corpus):
    name = "leveldb"

    def implementation(self):
        return self.project.one("Write", file="db/db_impl.cc", kind="Method", qualified="DBImpl::")

    def test_nominal_class_members_cross_header_and_implementation(self):
        method = self.implementation()
        node = self.project.one("DBImpl", file="db/db_impl.h", kind="Class", unit=method["unit"])
        methods = self.project.query("members", subject=node["id"], kind="Method", file="db/db_impl.cc", limit=2)
        self.assertTrue({"Write", "Get", "CompactMemTable"}.issubset({m["name"] for m in methods}))
        self.assertTrue(all(m["qualified_name"].startswith("leveldb::DBImpl::") for m in methods))

    def test_native_inheritance_points_to_db(self):
        method = self.implementation()
        node = self.project.one("DBImpl", file="db/db_impl.h", kind="Class", unit=method["unit"])
        bases = self.project.query("relations", subject=node["id"], relation="Extends")
        self.assertTrue(any(r["entity"]["qualified_name"] == "leveldb::DB" for r in bases))

    def test_status_definition_navigation_reaches_selected_translation_unit(self):
        method = self.implementation()
        declaration = self.project.one("ToString", file="include/leveldb/status.h", kind="Method", unit=method["unit"])
        definitions = self.project.query("definition", subject=declaration["id"])
        self.assertTrue(any(r.get("entity", r)["file"].endswith("util/status.cc") for r in definitions))

    def test_nested_iterator_is_not_a_top_level_class(self):
        rows = self.project.find("CompactionState", file="db/db_impl.cc", kind="Struct")
        self.assertTrue(rows)
        self.assertTrue(all("DBImpl::CompactionState" in r["qualified_name"] for r in rows))


class CJSON(Corpus):
    name = "cjson"

    def test_linked_json_node_members_are_fields(self):
        parse = self.project.one("cJSON_Parse", file="cJSON.c", kind="Function")
        node = self.project.one("cJSON", file="cJSON.h", kind="Struct", unit=parse["unit"])
        fields = self.project.query("members", subject=node["id"], kind="Field", limit=2)
        self.assertTrue({"next", "prev", "child", "type", "valuestring"}.issubset({f["name"] for f in fields}))

    def test_static_parser_helper_resolves_within_its_translation_unit(self):
        parse = self.project.one("cJSON_ParseWithLengthOpts", file="cJSON.c", kind="Function")
        callees = self.project.calls(parse)
        self.assertTrue(any(r["entity"]["name"] == "parse_value" and r["entity"]["file"].endswith("cJSON.c") for r in callees))

    def test_same_symbol_in_two_translation_units_stays_scoped(self):
        functions = self.project.find("cJSON_IsString", kind="Function")
        self.assertGreaterEqual(len({f["unit"] for f in functions}), 2)
        self.assertGreaterEqual(len({f["id"] for f in functions}), 2)


if __name__ == "__main__":
    classes = [c for c in [Ripgrep, Gin, LevelDB, CJSON] if not ARGS.only or c.name in ARGS.only]
    suite = unittest.TestSuite(unittest.defaultTestLoader.loadTestsFromTestCase(c) for c in classes)
    result = unittest.TextTestRunner(verbosity=2, resultclass=EvidenceResult).run(suite)
    (ARGS.output / "results.json").write_text(json.dumps({"tests": result.testsRun,
        "passed": result.wasSuccessful(), "cases": result.cases, "lock": LOCK}, indent=2))
    sys.exit(not result.wasSuccessful())
