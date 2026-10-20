import json
import os
import shutil
import sys
import tempfile
import types
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import mock_systemone  # noqa: E402
import run_judge  # noqa: E402
from judges import gemma_v1, generative  # noqa: E402

ROWS = [{"id": f"t-{i}", "language": "python", "comment_kind": "inline", "comment": "# hi",
         "annotated_code": "x = 1"} for i in range(6)]


class RunJudge(unittest.TestCase):
    def test_flash_against_mock(self):
        srv, url = mock_systemone.serve()
        tmp = tempfile.mkdtemp()
        gold = os.path.join(tmp, "gold.jsonl")
        with open(gold, "w") as f:
            f.write("".join(json.dumps(r) + "\n" for r in ROWS))
        name = "unittest-flash"
        out = os.path.join(HERE, "runs", name)
        shutil.rmtree(out, ignore_errors=True)
        try:
            args = ["--judge", "flash", "--gold", gold, "--endpoint", url, "--name", name,
                    "--append-question", "Is the code written in a compiled language?"]
            self.assertEqual(run_judge.main(args), 0)
            files = os.listdir(out)
            self.assertEqual(len(files), 6)
            rec = json.load(open(os.path.join(out, files[0])))
            self.assertEqual(rec["answers"]["hazard"], {"true": 0.5, "false": 0.5})
            self.assertEqual(set(rec["answers"]["c2_info_gain"]), {"0", "1", "2", "3"})
            self.assertIn("extra_question", rec["answers"])
            self.assertGreater(rec["latency_ms"], 0)
            before = os.path.getmtime(os.path.join(out, files[0]))
            self.assertEqual(run_judge.main(args), 0)  # resumes: nothing redone
            self.assertEqual(os.path.getmtime(os.path.join(out, files[0])), before)
        finally:
            srv.shutdown()
            shutil.rmtree(out, ignore_errors=True)
            shutil.rmtree(tmp)


class Parsers(unittest.TestCase):
    def test_gemma_lines(self):
        m = gemma_v1.LINE.match("MISS t-1      expected: NOT_A_REAL_LABEL" + " " * 44 + " got: DELETE")
        self.assertEqual(m.groups(), ("t-1", "DELETE"))
        row = {"comment": "# The lock is held\n# here"}
        self.assertEqual(gemma_v1.to_result(row, "DELETE")["verdict"], "delete")
        self.assertEqual(gemma_v1.to_result(row, "The lock is held here")["verdict"], "keep")
        r = gemma_v1.to_result(row, "lock held")
        self.assertEqual((r["verdict"], r["rewrite"]), ("rewrite_shorter", "lock held"))

    def test_generative_parse(self):
        text = 'thinking {"x": 1}\n```json\n{"labels": {"verdict": "delete", "hazard": false, "c5_contract": null}}\n```'
        labels = generative.parse_labels(text)
        self.assertEqual(labels["verdict"], "delete")
        self.assertEqual(generative.one_hot(labels), {"verdict": {"delete": 1.0}, "hazard": {"false": 1.0}})


if __name__ == "__main__":
    unittest.main()
