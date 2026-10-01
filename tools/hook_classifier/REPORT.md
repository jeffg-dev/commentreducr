# Python hook classifier experiment

Local classification is feasible within the proposed model budget. MiniLM exports
to 24 MB and ModernBERT to 155 MB, with warm single-block inference around 2 ms
and 14 ms respectively on this machine. ModernBERT is worth considering, but these
experiments do not establish a reliable blocking hook. Label interpretation and
quantization fidelity need more work.

The experiment flags an entire Python comment or docstring if any passage violates
the rubric. It generates no replacements and gives generic feedback. All quality
numbers below measure agreement with Luna annotations on synthetic examples,
rather than human-validated accuracy on real changes.

## Data and labeling

Claude Sonnet generated four variants for each of 240 Python scenarios, covering
comments and function, class, module, and test docstrings. The installed CLI
resolved `sonnet` to `claude-sonnet-5-5`. Python AST/token validation accepted
954 examples and rejected six.

The initial Luna labeling procedure used explicit FLAG maps and assigned PASS
to unlisted rows. That shortcut missed disallowed passages, including historical
implementation descriptions mixed with useful contracts. The first-run labels and
results are preserved as `*_initial.jsonl` and `*_initial.json`; their quality
metrics are superseded.

Three fresh GPT-6 Luna subagents then explicitly judged all 954 main examples and
all 134 curated examples. Each row, including PASS, received a specific rationale.
Reviewers saw only ID, kind, target text, and context, without prior labels,
generation prompts, or model results. Scripts validated IDs, schema, exact evidence,
and input fingerprints; they did not assign verdicts. This changed 164 main-corpus
and 16 curated verdicts.

The revised main corpus contains 425 FLAG and 529 PASS labels: 173 primary narration
violations, 172 noise violations, and 80 leaky references. The separate curated
set contains 106 FLAG and 28 PASS labels. It comes from earlier handcrafted
synthetic Python datasets and supplies less surrounding code.

A further blind audit agreed on 55 of 64 verdicts, or 85.9%. It deliberately sampled
equal numbers of PASS and FLAG from the two other reviewers and is not population
representative. Labels stayed frozen during that audit and training. Disagreements
concentrate on introductory summaries alongside useful contracts and the boundary
between necessary ordering constraints and named sibling references. Individual
judgment fixes the default-PASS procedure; it does not eliminate rubric ambiguity.
[Review provenance and disagreements](explicit_review.json)

## Training and evaluation

The unchanged split has 670 training, 142 development, and 142 test examples.
Scenario variants stay together, and identical normalized prose would link
scenarios before splitting. There is no cross-fold scenario or exact normalized
prose overlap. Related topics still occur across folds, so this is a limited
synthetic benchmark.

Inputs contain only kind, complete target text, and Python context with the target
removed. Labels, generator identity, scenario IDs, and style intent are excluded.
All targets and context fit the 384-token budget. The curated set never selects
checkpoints or thresholds.

A TF-IDF logistic regression supplies a cheap reference. MiniLM and ModernBERT
fine-tune every encoder parameter for three epochs, with the same inputs, split,
seed, batch size, and learning rates. MiniLM uses four CPU threads and ModernBERT
uses eight. Their fresh training runs took 219 and 589 seconds while sharing CPU.
Both selected epoch three using development data.

Development data selects a cutoff to maximize recall at at least 95% observed flag
precision. A predeclared 99% sensitivity cutoff is also recorded. These targets
are empirical selection criteria, not guarantees for unseen data.

The first export revealed that dynamic quantization could change decisions with
batch composition. Replacement exports record `inference_batch_size: 1`; both
development threshold selection and inference process each block independently.
All 276 test and curated rows produced identical scores and verdicts through the
singleton and multi-row API calls under this fixed policy.

This is an exploratory repeat: the initial test fold had already been inspected
when labeling and inference problems were discovered. The mixed export below was
motivated by observed quantization drift. Every new threshold still uses development
data only, but these results do not replace a fresh external evaluation.

## Classification results

Precision is the fraction of emitted flags matching a FLAG annotation. Recall is
the fraction of annotated violations caught. The test set contains 66 FLAG and
76 PASS examples; the curated set contains 106 FLAG and 28 PASS examples.

| Classifier | Test precision | Test recall | Test TP / FP | Curated precision | Curated recall | Curated TP / FP |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| TF-IDF | 100% | 12.1% | 8 / 0 | 90.0% | 8.5% | 9 / 1 |
| MiniLM int8 | 87.9% | 43.9% | 29 / 4 | 80.0% | 18.9% | 20 / 5 |
| ModernBERT int8 | 95.0% | 28.8% | 19 / 1 | 86.1% | 29.2% | 31 / 5 |
| ModernBERT mixed | 95.2% | 30.3% | 20 / 1 | 88.2% | 28.3% | 30 / 4 |
| ModernBERT float reference | 90.6% | 43.9% | 29 / 3 | 91.7% | 41.5% | 44 / 4 |

ModernBERT int8 favors test precision over MiniLM's higher test recall and improves
curated recall, but its curated precision falls below the development target.
At these operating points both models still miss most annotated violations.
Observed precision is uncertain: MiniLM's test 95% Wilson interval is approximately
72.7–95.2%, and ModernBERT int8's is 76.4–99.1%. These simple intervals omit
scenario clustering and labeling uncertainty.

Quantizing MatMul and Gather changes ModernBERT test scores by up to 0.478 and changes
14 of 142 verdicts compared with the float model at their separately selected
cutoffs. Keeping embeddings in float32 and quantizing only MatMul grows the bundle
to 271 MB and shifts only a few decisions. It does not recover the float model's
curated recall.

For the three exported neural variants, the 99% development cutoff coincides with
the 95% cutoff. Float ModernBERT's stricter cutoff gives 95.8% test precision at
34.8% recall and 96.7% curated precision at 27.4% recall. The float ONNX graph is
599 MB before tokenizer/configuration, above the proposed download budget.

Scores are uncalibrated and should not be shown as confidence percentages. Counts
and all failure examples are retained in [baseline and MiniLM results](results.json),
[ModernBERT results](results_modernbert.json), and
[mixed export results](results_modernbert_mixed.json).

## CPU performance and footprint

Measurements use an AMD Ryzen 9 9950X on Linux, after training finished, with three
fresh processes per exported model. First-result time includes process startup
and loading, with potentially warm OS file caches. Warm timings include tokenization.
Resident memory is sampled and is not a guaranteed maximum. These measurements
cover classification, not Git diff handling or repository parsing.

| Classifier | Bundle size | First result | Warm single block | Memory after load | Sampled peak with 20 blocks |
| --- | ---: | ---: | ---: | ---: | ---: |
| TF-IDF | 0.85 MB | 507 ms | 0.62 ms | 140 MiB | 141 MiB |
| MiniLM int8 | 24.07 MB | 113 ms | 2.15 ms | 94 MiB | 109 MiB |
| ModernBERT int8 | 154.99 MB | 363 ms | 14.31 ms | 264 MiB | 282 MiB |
| ModernBERT mixed | 271.03 MB | 424 ms | 13.70 ms | 359 MiB | 377 MiB |

Exported inference needs ONNX Runtime, tokenizers, and NumPy. All three neural
bundles ran with PyTorch, Transformers, and scikit-learn imports actively blocked.
There is no inference server or network requirement after export. Runtime packages
add to download size; the table reports model bundles. Slower laptops and other
operating systems remain unmeasured.

## Other models worth considering

ModernBERT-base is a relevant alternative because its pretraining includes English
and code. Its approximately 149-million-parameter encoder supports classification
fine-tuning. This experiment confirms CPU export within the model budget, with
quality and quantization limitations.
[ModernBERT model card](https://huggingface.co/answerdotai/ModernBERT-base)

DeBERTa-v3-xsmall would be my next untested comparison. It has a 22-million-parameter
backbone plus 48 million embedding parameters, about 70 million total. Its published
natural-language understanding results motivate a semantic classifier experiment.
Arithmetic suggests approximately 280 MB of float32 weights before export/tokenizer
overhead, potentially fitting the budget without int8 conversion. That estimate
is not a measured bundle size or performance result.
[DeBERTa model card](https://huggingface.co/microsoft/deberta-v3-xsmall)

CodeBERT is another candidate for relating prose to implementation because it was
pretrained jointly on programming and natural language. Its suitability for this
rubric and CPU footprint remain untested here.
[CodeBERT paper](https://arxiv.org/abs/2002.08155)

## Recommended next step

First clarify the disputed boundaries with a few explicit PASS/FLAG examples,
then create a human-reviewed evaluation set from real Python changes, holding out
entire repositories. Expand narration and mixed-content cases with close pairs:
the same code and useful contract, with one redundant or historical passage added.
This directly tests the whole-block rule and limits shortcuts based on writing style.

Retain both MiniLM and ModernBERT as comparison models. DeBERTa-v3-xsmall offers
a useful next architecture comparison; calibrated quantization is another route
to test if ModernBERT's float behavior remains attractive. The current mixed export
offers little benefit for its additional size.

Fine-tuning a pretrained encoder already gives us a classifier dedicated to this
task. My recommendation is to improve label consistency and real-code evaluation,
then consider distillation into a smaller model. The present experiment supplies
little evidence for training language representations from scratch.

The branch contains corpus generation, annotations, frozen splits, training,
export, offline inference, and evaluation records. Changed-file selection,
structural exemptions, long-block handling, and `commentreducr --hook` integration
remain future work. Reproduction is documented in [README.md](README.md).
