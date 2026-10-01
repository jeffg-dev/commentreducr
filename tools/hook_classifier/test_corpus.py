import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from corpus import assemble, generate, generate_batch, validate_example


class CorpusValidationTests(unittest.TestCase):
    def test_removes_only_the_actual_docstring_from_context(self):
        code = 'def read():\n    """Flush before closing.\n\n    Closing discards buffered writes.\n    """\n    return "Flush before closing."\n'
        row = validate_example({"kind": "function_docstring", "code": code,
                                "text": "Flush before closing.\n\nClosing discards buffered writes."})
        self.assertEqual(row["code"], code)
        self.assertEqual(row["context"], 'def read():\n    return "Flush before closing."')

    def test_rejects_a_string_that_is_not_a_docstring(self):
        with self.assertRaises(ValueError):
            validate_example({"kind": "function_docstring", "code": 'def read():\n    x = 1\n    "flush first"\n    return x\n', "text": "flush first"})

    def test_requires_the_entire_comment_block(self):
        code = '# First sentence.\n# Second sentence.\nvalue = 1\n'
        with self.assertRaises(ValueError):
            validate_example({"kind": "comment", "code": code, "text": "# First sentence."})
        row = validate_example({"kind": "comment", "code": code,
                                "text": "# First sentence.\n# Second sentence."})
        self.assertEqual(row["context"], "value = 1")

    def test_rejects_stale_cache_without_calling_claude(self):
        with tempfile.TemporaryDirectory() as directory:
            raw = Path(directory)
            (raw / "batch_000.json").write_text(json.dumps({"result": "{}"}))
            with patch("corpus.subprocess.run") as run:
                with self.assertRaisesRegex(ValueError, "request provenance"):
                    generate_batch(0, [], raw, 1)
                run.assert_not_called()

    def test_rejects_incomplete_generator_responses(self):
        requested = [{"scenario_id": "py-0000", "kind": "comment"}]
        for payload in [{"scenarios": []}, {"scenarios": [{"scenario_id": "py-0000", "examples": []}]}]:
            with self.subTest(payload=payload), tempfile.TemporaryDirectory() as directory:
                response = json.dumps({"result": json.dumps(payload)})
                with patch("corpus.subprocess.run") as run:
                    run.return_value.returncode = 0
                    run.return_value.stdout = response
                    run.return_value.stderr = ""
                    with self.assertRaisesRegex(ValueError, "missing scenarios|four examples"):
                        generate_batch(0, requested, Path(directory), 1)

    def test_failed_generation_preserves_existing_corpus(self):
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory)
            corpus = destination / "corpus.jsonl"
            frozen = '{"frozen": true}\n'
            corpus.write_text(frozen)
            args = SimpleNamespace(output_dir=destination, scenarios=1,
                                   batch_scenarios=1, workers=1, timeout=1)
            with patch("corpus.generate_batch", side_effect=ValueError("incomplete response")):
                with self.assertRaises(SystemExit):
                    generate(args)
            self.assertEqual(corpus.read_text(), frozen)
            self.assertTrue((destination / "raw/generation.failed.json").exists())

    def test_evidence_string_cannot_pass_as_an_array_of_characters(self):
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory)
            corpus = destination / "corpus.jsonl"
            labels = destination / "labels.jsonl"
            corpus.write_text(json.dumps({"id": "a", "kind": "comment",
                                         "text": "# implementation steps", "context": "run()"}) + "\n")
            labels.write_text(json.dumps({"id": "a", "verdict": "FLAG", "rules": ["narration"],
                                         "evidence": "implementation", "reason": "Narrates implementation.",
                                         "labeler_model": "gpt-6-luna"}) + "\n")
            args = SimpleNamespace(corpus=corpus, labels=[labels], output=destination / "out.jsonl")
            with self.assertRaisesRegex(ValueError, "must be arrays"):
                assemble(args)


if __name__ == "__main__":
    unittest.main()
