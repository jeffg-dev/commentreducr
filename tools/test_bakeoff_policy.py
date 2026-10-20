import json, os, sys, tempfile, unittest

T = os.path.dirname(os.path.abspath(__file__))
sys.path[:0] = [os.path.join(T, "bakeoff"), os.path.join(T, "dataset", "v2")]
import check, policy, score, tune  # noqa: E402
from split import split_of  # noqa: E402

TH = dict(policy.DEFAULT_THRESHOLDS, use_holistic=False)


def score_dist(level):
    return {str(i): 1.0 if i == level else 0.0 for i in range(4)}


def noul(p):
    return {"true": p, "false": 1 - p}


def answers(c2=2, hazard=0.9, c3=2, g1=0.0, fully=0.0, leak=0.0):
    return {"g1_contradicts": noul(g1), "c2_info_gain": score_dist(c2), "hazard": noul(hazard),
            "c3_rationale": score_dist(c3), "leak": noul(leak),
            "c9_replaceable": {"fully": fully, "partly": 0.0, "no": 1 - fully}}


def row(kind="inline", comment="# x", **lab):
    labels = {"verdict": "keep", "hazard": False, "g1_contradicts": False, "g1_unverifiable": False,
              "leak": False, "kind": "rationale", "c4_level": "mental_model", "c9_replaceable": "no",
              "c2_info_gain": 2, "c3_rationale": 2, "c8_durability": 2, "c5_contract": None,
              "c10_actionable": None, "rewrite": None, "rename_suggestion": None, **lab}
    code = "x = 1"
    return {"id": "a", "language": "python", "comment_kind": kind, "placement": "trailing", "comment": comment,
            "annotated_code": code, "enclosing_scope": "def f():\n    " + code, "labels": labels, "claims": []}


class Policy(unittest.TestCase):
    def test_inline_paths(self):
        r = row()
        self.assertEqual(policy.decide(answers(), r, TH), "keep")
        self.assertEqual(policy.decide(answers(c2=0), r, TH), "delete")
        self.assertEqual(policy.decide(answers(g1=1.0), r, TH), "fix_wrong_claim")
        self.assertEqual(policy.decide(answers(fully=1.0), r, TH), "rename_instead")
        self.assertEqual(policy.decide(answers(hazard=0.0, c3=0), r, TH), "delete")
        self.assertEqual(policy.decide(answers(), row(comment="a\nb\nc\nd"), TH), "rewrite_shorter")

    def test_todo_directive_keep(self):
        self.assertEqual(policy.decide({}, row(kind="todo"), TH), "keep")
        self.assertEqual(policy.decide({}, row(kind="directive"), TH), "keep")

    def test_abstain(self):
        r = row()
        self.assertEqual(policy.decide(answers(g1=0.65), r, dict(TH, g1=0.6, margin=0.15)), "review")
        self.assertEqual(policy.decide({}, r, TH), "review")  # missing questions
        a = answers()
        a["verdict"] = {"keep": 0.1, "delete": 0.9}
        self.assertEqual(policy.decide(a, r, dict(TH, use_holistic=True)), "review")
        self.assertEqual(policy.decide(a, r, TH), "keep")

    def test_shared_cases(self):
        # The same file runs through src/policy.rs in policy::tests::shared_cases.
        path = os.path.join(T, "..", "tests", "fixtures", "policy", "cases.json")
        for c in json.load(open(path)):
            th = dict(policy.DEFAULT_THRESHOLDS, **c["thresholds_override"])
            r = {k: c[k] for k in ("comment_kind", "comment", "annotated_code") if k in c}
            got = policy.decide(c["answers"], r, th)
            self.assertEqual(got, c["expected"], c["name"])

    def test_generative_bypass(self):
        self.assertEqual(policy.judge_verdict({"answers": {}, "verdict": "delete"}, row(), TH), "delete")
        one_hot = {"judge": "generative", "answers": answers(), "verdict": "delete"}
        self.assertEqual(policy.judge_verdict(one_hot, row(), TH), "delete")
        for v in ("remove", "Delete", "review", None):
            self.assertEqual(policy.judge_verdict(dict(one_hot, verdict=v), row(), TH), "review")
        unit = dict(row(comment="// 5 seconds"), annotated_code="gcTime: 5000,")
        self.assertEqual(policy.judge_verdict(one_hot, unit, TH), "keep")  # the unit guard holds here too


class Score(unittest.TestCase):
    def test_flags_and_cost(self):
        hz = row(hazard=True, verdict="keep")
        self.assertTrue(score.flags(hz, "delete")["hazard_lost"])
        self.assertEqual(score.cost(score.flags(hz, "delete")), 10)
        self.assertEqual(score.cost(score.flags(hz, "review")), 0.5)
        self.assertEqual(score.cost(score.flags(row(), "rewrite_shorter")), 1)  # churn
        self.assertEqual(score.cost(score.flags(row(verdict="delete"), "keep")), 0.3)

    def test_macro_f1(self):
        self.assertEqual(score.macro_f1([("keep", "keep"), ("delete", "delete")]), 1.0)
        self.assertEqual(score.macro_f1([("keep", "delete"), ("delete", "keep")]), 0.0)

    def test_end_to_end_and_tune(self):
        with tempfile.TemporaryDirectory() as d:
            rows = []
            for i in range(40):
                r = row(verdict="delete" if i % 2 else "keep")
                r["id"] = f"t{i}"
                rows.append(r)
                os.makedirs(f"{d}/runs/j", exist_ok=True)
                json.dump({"id": r["id"], "judge": "j", "answers": answers(c2=0 if i % 2 else 2),
                           "verdict": None, "rewrite": None, "latency_ms": 10.0, "input_tokens": 100, "raw": {}},
                          open(f"{d}/runs/j/t{i}.json", "w"))
            open(f"{d}/gold.jsonl", "w").write("\n".join(map(json.dumps, rows)))
            gold = score.load_gold(f"{d}/gold.jsonl", "all")
            rep = score.score(gold, score.load_runs(f"{d}/runs", "j"), TH)
            self.assertEqual(rep["overall"]["macro_f1"], 1.0)
            self.assertEqual(rep["overall"]["questions"]["hazard"]["n"], 40)
            dev = [(r, score.load_runs(f"{d}/runs", "j")[r["id"]]) for r in gold if split_of(r["id"]) == "dev"]
            self.assertEqual(tune.tune(dev)[0], 0)


class Check(unittest.TestCase):
    def test_validate(self):
        good = row()
        self.assertEqual(check.validate(good), [])
        bad = row(verdict="rewrite_shorter", c2_info_gain=7)
        bad["claims"] = [{"text": "t", "status": "contradicted"}]
        bad["enclosing_scope"] = "other"
        msgs = " | ".join(check.validate(bad))
        for s in ("rewrite required", "c2_info_gain", "g1_contradicts disagrees", "does not contain"):
            self.assertIn(s, msgs)
        self.assertTrue(any("c5_contract" in m for m in check.validate(row(kind="api_doc"))))
        both = row(g1_contradicts=True, verdict="delete")
        both["claims"] = [{"text": "a", "status": "contradicted"}, {"text": "b", "status": "unverifiable"}]
        self.assertEqual(check.validate(both), [])


if __name__ == "__main__":
    unittest.main()
