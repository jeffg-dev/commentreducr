import json
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).parent
RUST_FIXTURES = HERE.parent.parent / "tests" / "fixtures" / "render"
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parent / "dataset" / "v2"))

from render import DEFAULT_SCHEMA, questions_for, render_state, render_verify  # noqa: E402
from split import split_of  # noqa: E402

LABELS = ["kind", "g1_contradicts", "g1_unverifiable", "hazard", "c2_info_gain", "c3_rationale",
          "c4_level", "c8_durability", "c9_replaceable", "leak", "c5_contract", "c10_actionable",
          "verdict"]

ROW = {
    "id": "synthetic-0001",
    "language": "python",
    "comment_kind": "inline",
    "placement": "before_statement",
    "comment": "# hold the lock: the refresher thread writes _cache",
    "annotated_code": "with self._lock:\n    self._cache[k] = v",
    "enclosing_scope": "\n".join([f"    x{i} = {i}" for i in range(200)]
                                 + ["    with self._lock:", "        self._cache[k] = v"]
                                 + [f"    y{i} = {i}" for i in range(200)]),
    "symbol": {"name": "Cache.set", "public": False, "is_test": False},
    "change": {"authored_by_agent": True, "agent": "claude-code", "comment_age_days": None,
               "code_newer_than_comment": False},
}


class RenderTest(unittest.TestCase):
    def test_order_and_evidence(self):
        text = render_state(ROW)
        order = [text.index(s) for s in ("language: python", "comment:", "annotated_code:",
                                          "symbol: Cache.set", "evidence:", "enclosing_scope:")]
        self.assertEqual(order, sorted(order))
        self.assertIn("agent: claude-code", text)
        self.assertNotIn("comment_age_days", text)

    def test_change_record(self):
        plain = render_state(ROW)
        self.assertNotIn("annotated_code_before", plain)
        text = render_state({**ROW, "change": {"scope_changed": True, "annotated_code_before": "x = 0"}})
        self.assertIn("evidence:\nscope_changed: True", text)
        self.assertLess(text.index("scope_changed"), text.index("annotated_code_before:\nx = 0"))
        self.assertLess(text.index("annotated_code_before"), text.index("enclosing_scope:"))

    def test_trims_scope_around_code(self):
        text = render_state(ROW, max_tokens=300)
        self.assertLessEqual(len(text) // 4, 300)
        self.assertIn("with self._lock:", text.split("enclosing_scope:")[1])
        self.assertNotIn("x0 = 0", text)

    def test_questions_for(self):
        schema = json.loads(DEFAULT_SCHEMA.read_text())
        self.assertEqual(sorted(schema["questions"]), sorted(LABELS))
        para = json.loads((HERE / "schema" / "strict-v0-paraphrase.json").read_text())
        for qid, q in schema["questions"].items():
            p = para["questions"][qid]
            self.assertEqual(q["type"], p["type"])
            crit = q.get("criteria", [])
            self.assertEqual(list(crit) if isinstance(crit, dict) else len(crit),
                             list(p.get("criteria", [])) if isinstance(crit, dict) else len(p.get("criteria", [])))
        qs = questions_for(schema, ROW)
        self.assertNotIn("c5_contract", qs)
        self.assertNotIn("c10_actionable", qs)
        self.assertIn("c5_contract", questions_for(schema, {"comment_kind": "api_doc"}))

    def test_write_rust_fixtures(self):
        # Writes the expected outputs that render::tests::parity in Rust compares against.
        schema = json.loads(DEFAULT_SCHEMA.read_text())
        for case in json.loads((RUST_FIXTURES / "cases.json").read_text()):
            name, row = case["name"], case["row"]
            (RUST_FIXTURES / f"{name}.state.txt").write_text(render_state(row, case["max_tokens"]), newline="")
            qs = json.dumps(questions_for(schema, row), indent=1, ensure_ascii=False) + "\n"
            (RUST_FIXTURES / f"{name}.questions.json").write_text(qs)
        for c in json.loads((RUST_FIXTURES / "verify.json").read_text()):
            text = render_verify(c["original"], c["candidate"], c["annotated_code"], c["enclosing_scope"],
                                 c["max_tokens"])
            (RUST_FIXTURES / f"{c['name']}.verify.txt").write_text(text, newline="")
        stale = json.loads((DEFAULT_SCHEMA.parent / "stale-v2.json").read_text())
        qs = questions_for(stale, {})
        self.assertTrue(qs["g1_contradicts"]["instructions"].startswith(stale["instructions"] + " "))
        (RUST_FIXTURES / "stale.questions.json").write_text(json.dumps(qs, indent=1, ensure_ascii=False) + "\n")

    def test_render_verify(self):
        text = render_verify("# a b", "# a", "x = 1", "\n".join(f"y{i} = {i}" for i in range(400)) + "\nx = 1",
                             max_tokens=100)
        order = [text.index(s) for s in ("original:", "candidate:", "annotated_code:", "enclosing_scope:")]
        self.assertEqual(order, sorted(order))
        self.assertLessEqual(len(text) // 4, 100)
        self.assertIn("x = 1", text.split("enclosing_scope:")[1])


class SplitTest(unittest.TestCase):
    def test_split(self):
        counts = {}
        for i in range(1000):
            counts[split_of(f"id-{i}")] = counts.get(split_of(f"id-{i}"), 0) + 1
        self.assertEqual(set(counts), {"train", "dev", "test"})
        self.assertTrue(450 < counts["train"] < 550 and 150 < counts["dev"] < 250)


if __name__ == "__main__":
    unittest.main()
