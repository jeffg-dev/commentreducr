"""Checks the experiment's leakage barriers and conservative threshold behavior."""

import unittest

from infer import model_input
from train import choose_threshold, join_labels, split_rows


class DatasetSafetyTests(unittest.TestCase):
    def test_scenarios_and_repeated_prose_cannot_cross_folds(self):
        rows = [
            {"id": f"{scenario}-{style}", "scenario_id": f"scenario-{scenario}",
             "kind": "comment", "text": f"Text {scenario} style {style}",
             "annotation": {"verdict": "PASS" if style else "FLAG"}}
            for scenario in range(12) for style in range(3)
        ]
        rows[3]["text"] = " TEXT 0 STYLE 0 "
        split, checks = split_rows(rows, 42)
        lookup = {row["id"]: name for name, fold in split.items() for row in fold}
        self.assertEqual(lookup["0-0"], lookup["1-0"])
        for scenario in range(12):
            self.assertEqual(len({lookup[f"{scenario}-{style}"] for style in range(3)}), 1)
        self.assertEqual(checks["cross_fold_normalized_prose_overlap"], 0)
        self.assertEqual(split, split_rows(rows, 42)[0])

    def test_input_allowlist_excludes_label_and_generator_metadata(self):
        row = {"kind": "comment", "text": "Wait before retrying.", "context": "retry()",
               "scenario_id": "secret scenario", "style": "FLAG", "label": 1}
        self.assertNotIn("secret", model_input(row))
        self.assertNotIn("FLAG", model_input(row))

    def test_threshold_does_not_claim_precision_when_nothing_is_flagged(self):
        threshold, chosen = choose_threshold([{"label": 0}, {"label": 1}], [0.9, 0.8], 0.95)
        self.assertGreater(threshold, 1)
        self.assertFalse(chosen["achieved_nonzero_dev_flags"])
        self.assertIsNone(chosen["dev"]["flag_precision"])

    def test_threshold_uses_dev_recall_under_precision_constraint(self):
        rows = [{"label": label} for label in [0, 1, 1, 0]]
        threshold, chosen = choose_threshold(rows, [0.2, 0.9, 0.7, 0.6], 0.95)
        self.assertAlmostEqual(threshold, 0.65)
        self.assertEqual(chosen["dev"]["flag_recall"], 1)

    def test_missing_or_duplicate_annotations_are_rejected(self):
        with self.assertRaises(ValueError):
            join_labels([{"id": "a"}], [])
        with self.assertRaises(ValueError):
            join_labels([{"id": "a"}], [{"id": "a"}, {"id": "a"}])


if __name__ == "__main__":
    unittest.main()
