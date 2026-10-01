"""Guard the fixed inference policy used to select quantized thresholds."""

from types import SimpleNamespace
import unittest

import numpy as np

from infer import Classifier


class InferencePolicyTests(unittest.TestCase):
    def test_singleton_policy_keeps_other_blocks_out_of_the_model_call(self):
        seen = []

        def encode(rows):
            seen.append(len(rows))
            return [SimpleNamespace(ids=[1], attention_mask=[1], type_ids=[0]) for _ in rows]

        runtime = Classifier.__new__(Classifier)
        runtime.np = np
        runtime.baseline = None
        runtime.config = {"inference_batch_size": 1}
        runtime.tokenizer = SimpleNamespace(encode_batch=encode)
        runtime.session = SimpleNamespace(
            get_inputs=lambda: [SimpleNamespace(name="input_ids")],
            run=lambda outputs, feed: [np.zeros((len(feed["input_ids"]), 2))],
        )
        rows = [{"kind": "comment", "text": "# Contract", "context": "run()"}] * 3
        np.testing.assert_array_equal(runtime.probabilities(rows), [0.5, 0.5, 0.5])
        self.assertEqual(seen, [1, 1, 1])


if __name__ == "__main__":
    unittest.main()
