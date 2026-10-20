# Changeset validation on agent-authored PRs (Phase 4 exit check)

Date: 2026-10-19.

## Verdict

**The Phase 4 exit criterion is not met.** The criterion (`docs/PLAN.md`, Phase 4) is that no
tier-2 finding on a still-correct comment would have deleted it.

- 79 of the 83 tier-2 findings flag a comment that is still accurate and useful. Acting on any
  of them would remove a good comment.
- 7 of those 79 carry the verdict `stale`, the kind that reaches a rewrite or delete. That
  matches the blocker as `docs/plan-phase4.md` defines it ("any `stale` finding on a comment
  that is still correct"). All 7 `stale` findings are false.
- The other 72 carry `stale?` (possibly stale), which is a note, not an edit. Its precision
  is 2 of 76, about 0.03, against a target of at least 0.5.
- Only 2 of 83 findings are correct. Both flag commented-out dead code, which the PR did not
  cause but which is fair to flag.
- 2 more findings are wrong without harming anything, because the comment was already low
  value or already wrong before the PR (details below).

The run is also incomplete: 18 of the 30 PRs finished. The criterion fails on those 18 alone,
so finishing the other 12 cannot change the verdict.

## What ran

- 30 public agent-authored PRs from the AIDev dataset, in repositories not used in the gold
  set. `check` ran report-only on each, from the merge-base of base and head to the head.
- Report-only wrote nothing. Every worktree was clean afterwards, and HEAD had not moved.
- 18 PRs finished. 12 got no verdicts because a judge endpoint was down. None of the 12 were
  commentreducr crashes:
  - 10 could not reach the fine-tuned judge server after it stopped answering (claude-relay-service,
    uv, HAsmartirrigation, TanStack db, reflex, datafusion, gobii, vscode, verifiers, onlook).
  - 2 timed out on the first request to the generative model that writes fixes (memfs,
    browser-use).
- Total runtime for the 18 was 1,899 seconds. The longest were oxc (381 s), liam (339 s) and
  llama-stack (267 s).
- 3 PRs had no changed comment blocks (glaredb, SQLPage, liam). crewAI checked 1 block and
  found nothing. async-code skipped one file that would not parse.
- The rewrite step rejected 10 candidate rewrites, in llama-stack, oxc and zcf.

## Findings on the 18 finished PRs

| Tier | Verdict | Findings |
|---|---|---|
| 1 | delete | 161 |
| 1 | review | 1 |
| 2 | stale? | 76 |
| 2 | stale | 7 |

Tier 1 was not checked by hand; this check covers tier 2 only. Only 2 of the 83 tier-2
findings had evidence that the comment was written by the agent.

## Hand check of every tier-2 finding

All 83 tier-2 findings were checked by reading the comment, about a dozen lines around it, and
the base-to-head diff of its file. No code was run.

| Result | Count |
|---|---|
| Correct | 2 |
| Wrong, would remove an accurate comment | 79 (7 `stale`, 72 `stale?`) |
| Wrong, harmless | 2 |
| Total | 83 |

Tier-2 findings came from 10 PRs: oxc #13030 (33), zcf #19 (19), llama-stack #3670 (12),
open-webui-developer-toolkit #24 (9), dot-ai #50 (4), ShiguReader #310 (2), and 1 each in
promptfoo #4345, async-code #2, label-studio #8548 and CodeceptJS #5077.

### Why they are wrong

One cause covers almost all of them: **the PR changed code near the comment, but nothing the
comment says.** The judge treats "code near this comment changed" as "this comment may be
stale". The nearby changes were:

- A signature-only edit. In oxc #13030 every one of the 33 findings sits next to a function whose
  context parameter changed from a mutable to a shared reference.
- A rename that the comment does not mention (llama-stack renamed a type variable; dot-ai
  renamed a result field).
- A new step, parameter, import or sibling function added elsewhere (zcf added a later step to
  a numbered list of steps, and every step comment still numbers correctly).

The 2 harmless wrong findings:

- ShiguReader #310, `ExplorerPage.js:777`: a leftover wish-list note that the PR did not make
  stale. Deleting it loses nothing.
- oxc #13030, `remove_dead_code.rs:491`: the comment's wording was already wrong before the
  PR. Calling it stale is the wrong reason, but no correct comment is lost.

### Each wrong finding

The cause column is the hand checker's one-line note.

### oxc-project/oxc #13030 (33)

| File:line | Verdict | Would remove an accurate comment | Cause |
|---|---|---|---|
| `fold_constants.rs:116` | stale? | yes | Still describes branch; PR only changed ctx to & reference. |
| `fold_constants.rs:123` | stale? | yes | Example transform still matches code; PR only made ctx & instead of &mut. |
| `fold_constants.rs:138` | stale | yes | Fold comment accurate; diff only changes ctx &mut to &. |
| `fold_constants.rs:156` | stale? | yes | PR only changes ctx &mut -> & across minifier; logical-expression examples still correct. |
| `fold_constants.rs:189` | stale | yes | Example still matches code. |
| `fold_constants.rs:198` | stale | yes | Example still matches code; only ctx borrow changed. |
| `fold_constants.rs:224` | stale | yes | Example accurate; no diff near. |
| `fold_constants.rs:359` | stale? | yes | Refactor of ctx type only; tryFoldAdd attribution still accurate. |
| `fold_constants.rs:372` | stale? | yes | Still matches code. |
| `fold_constants.rs:404` | stale? | yes | Example still matches code; only ctx borrow changed. |
| `fold_constants.rs:426` | stale? | yes | Quasi comment accurate; diff only changes ctx signature. |
| `fold_constants.rs:431` | stale? | yes | Template-literal fold example still matches code. |
| `fold_constants.rs:447` | stale? | yes | Still matches code. |
| `fold_constants.rs:466` | stale? | yes | Example still matches code; only ctx borrow changed. |
| `fold_constants.rs:675` | stale? | yes | __proto__ comment accurate; signature-only diff. |
| `fold_constants.rs:703` | stale? | yes | Getter-spread comment still accurate; only signature reformatted. |
| `fold_constants.rs:706` | stale? | yes | Still explains __proto__ check. |
| `minimize_logical_expression.rs:184` | stale? | yes | Note about with statements still valid; only ctx borrow changed. |
| `remove_dead_code.rs:478` | stale? | yes | Typeof example accurate; signature-only diff. |
| `remove_dead_code.rs:489` | stale? | yes | Delete foo.#a example still matches PrivateFieldExpression arm. |
| `remove_dead_code.rs:491` | stale? | no | Pre-existing copy-paste wording (typeof under Delete), not made stale by the PR; deleting it for staleness would be wrong reason. |
| `remove_dead_code.rs:494` | stale? | yes | Example cases still match the match arms; only signature formatting changed. |
| `remove_unused_declaration.rs:19` | stale? | yes | Direct eval comment accurate; signature-only diff. |
| `replace_known_methods.rs:78` | stale? | yes | Expect on checked len still accurate. |
| `replace_known_methods.rs:80` | stale? | yes | Low value but accurate; PR only changed ctx mutability. |
| `replace_known_methods.rs:92` | stale? | yes | Referenced fn is_binary_operator_that_does_number_conversion still exists; only ctx borrow changed. |
| `replace_known_methods.rs:207` | stale? | yes | Concat comment accurate; signature-only diff. |
| `replace_known_methods.rs:278` | stale? | yes | String::concat length heuristic comment still accurate. |
| `replace_known_methods.rs:305` | stale? | yes | Accurate; PR only changed signatures. |
| `replace_known_methods.rs:455` | stale? | yes | Comment still matches the pow_with_expr code; only ctx borrow changed. |
| `replace_known_methods.rs:463` | stale? | yes | Math comment accurate; signature-only diff. |
| `replace_known_methods.rs:475` | stale? | yes | 2**-52 EPSILON comment still accurate. |
| `substitute_alternate_syntax.rs:421` | stale? | yes | Still accurate, with-statement caveat unchanged. |

### UfoMiao/zcf #19 (19)

| File:line | Verdict | Would remove an accurate comment | Cause |
|---|---|---|---|
| `init.ts:61` | stale? | yes | Step 1 still language selection; new step added later at 11. |
| `init.ts:72` | stale? | yes | Step numbering intact; PR only added an import and a new Step 11 later. |
| `init.ts:93` | stale? | yes | Step 3 label is still correct; only a later step was added. |
| `init.ts:97` | stale? | yes | PR adds Step 11 at end; Step 4 numbering and meaning unchanged. |
| `init.ts:121` | stale? | yes | Step 5 still existing-config handling; numbering not disturbed. |
| `init.ts:152` | stale? | yes | Step 6 comment and code unchanged; numbering intact. |
| `init.ts:208` | stale | yes | Branch label still accurate; low value but not stale. |
| `init.ts:223` | stale? | yes | CCR branch untouched; comment still accurate. |
| `init.ts:275` | stale? | yes | Still true; CCR path unchanged by PR. |
| `init.ts:285` | stale? | yes | Step 7 comment and code unchanged. |
| `init.ts:294` | stale? | yes | Docs-only comment still accurate. |
| `init.ts:305` | stale? | yes | Step 8 numbering unchanged; PR adds only a later step. |
| `init.ts:307` | stale? | yes | Step 8.5 still personality config; unchanged. |
| `init.ts:310` | stale? | yes | Step 9 comment and code unchanged. |
| `init.ts:317` | stale? | yes | ConfigureApi still calls addCompletedOnboarding indirectly per repo notes; comment accurate. |
| `init.ts:321` | stale? | yes | Step 10 unchanged; PR adds Step 11 after. |
| `init.ts:341` | stale? | yes | Still uses common MCP selector; unchanged. |
| `init.ts:431` | stale? | yes | Step 12 follows new Step 11 correctly; numbering consistent. |
| `init.ts:438` | stale? | yes | Step 13 follows Step 12 correctly after new Step 11 insert; numbering consistent. |

### llamastack/llama-stack #3670 (12)

| File:line | Verdict | Would remove an accurate comment | Cause |
|---|---|---|---|
| `generator.py:726` | stale? | yes | Multipart branch comment still accurate; no diff near it. |
| `generator.py:749` | stale? | yes | PR only adds extra-body handling elsewhere; comment about enum/BaseModel/simple-type schema ref still true. |
| `generator.py:766` | stale? | yes | Elif request_params branch still JSON body; PR only added extra_body docs above. |
| `generator.py:907` | stale? | yes | Comment still accurate; diff only adds extraBodyParameters, description logic unchanged. |
| `operations.py:219` | stale | yes | Docstring still accurate; diff only adds a local variable. |
| `operations.py:317` | stale? | yes | PR adds extra_body param handling; Generator event-type comment unchanged and accurate. |
| `operations.py:417` | stale? | yes | Docstring accurate; PR only fixed whitespace and added a sibling function. |
| `agents.py:821` | stale? | yes | Still true: max_infer_iters is an OpenAI API extension; PR only added shields param. |
| `library_client.py:372` | stale? | yes | Assert note still true; diff adds unrelated extra_json merge. |
| `library_client.py:395` | stale? | yes | PR adds extra_json merge above; FastAPI Response handling comment still accurate. |
| `schema_utils.py:81` | stale | yes | Docstring still accurate; PR renamed TypeVar T to CallableT, params unchanged. |
| `schema_utils.py:110` | stale? | yes | Still accurate; PR only renamed type var T to CallableT, logic unchanged. |

### jrkropp/open-webui-developer-toolkit #24 (9)

| File:line | Verdict | Would remove an accurate comment | Cause |
|---|---|---|---|
| `openai_responses_api_pipeline.py:228` | stale? | yes | Httpx client swap does not affect logger propagate=False; accurate. |
| `openai_responses_api_pipeline.py:277` | stale? | yes | Still describes valve override loop; PR changed client code only. |
| `openai_responses_api_pipeline.py:283` | stale? | yes | Inherit-skip behaviour unchanged; PR edits are elsewhere. |
| `openai_responses_api_pipeline.py:325` | stale? | yes | Chat_id access comment unrelated to tool_ids change. |
| `openai_responses_api_pipeline.py:336` | stale? | yes | STEP 3 tool prep unchanged; accurate. |
| `openai_responses_api_pipeline.py:350` | stale? | yes | Step 4 still builds request_params; unchanged. |
| `openai_responses_api_pipeline.py:378` | stale? | yes | Loop logic and step label still correct; only the request call inside changed. |
| `openai_responses_api_pipeline.py:436` | stale? | yes | Still appends reasoning item for next turn; stream source change does not affect it. |
| `openai_responses_api_pipeline.py:591` | stale? | yes | Pending function-call section unchanged; accurate. |

### vfarcic/dot-ai #50 (4)

| File:line | Verdict | Would remove an accurate comment | Cause |
|---|---|---|---|
| `schema.ts:455` | stale? | yes | Step 1 comment accurate; diff only renames result.pattern to result.data. |
| `schema.ts:466` | stale? | yes | Step 2 concept pattern search unchanged; only result.pattern->result.data renamed. |
| `schema.ts:490` | stale? | yes | Still dedups; PR changed result.pattern to result.data above, not this step. |
| `organizational-data.ts:340` | stale? | yes | Code under comment unchanged; client-side slice still correct. |

### ObservedObserver/async-code #2 (1)

| File:line | Verdict | Would remove an accurate comment | Cause |
|---|---|---|---|
| `main.py:267` | stale? | yes | PR switches CLI by model; container.wait(timeout=300) still 5 minutes. |

### HumanSignal/label-studio #8548 (1)

| File:line | Verdict | Would remove an accurate comment | Cause |
|---|---|---|---|
| `VideoCanvas.tsx:608` | stale? | yes | Cleanup effect unchanged; PR only added speed prop elsewhere. Vague but not stale. |

### codeceptjs/CodeceptJS #5077 (1)

| File:line | Verdict | Would remove an accurate comment | Cause |
|---|---|---|---|
| `Playwright.js:2800` | stale? | yes | Timeout/two-strategy rationale still holds after adding cleanup flag; retry count change does not affect it. |

### hjyssg/ShiguReader #310 (1)

| File:line | Verdict | Would remove an accurate comment | Cause |
|---|---|---|---|
| `ExplorerPage.js:777` | stale? | no | Dangling wishlist note unrelated to the PR diff (fileName->filePath prop rename elsewhere); not made stale, but low value so deleting it would not lose useful info. |

## What this means

- The tier-2 signal as it stands is "code near the comment changed". On these PRs that is
  almost never evidence the comment is wrong (2 correct out of 83).
- The judge must see whether the change touches what the comment claims, not only that the
  change is nearby. Until it does, tier 2 should not produce `stale`.

## Not checked

- Tier 1 (162 findings) was not checked by hand.
- The 12 unfinished PRs. To finish them, restart the fine-tuned judge server and rerun the
  runner; it skips PRs already done.
- No code was run to confirm any call. One zcf call (`init.ts:317`) rests on lighter reading
  than the rest.

The PR list with URLs, the raw findings and the per-item checker output stay outside the repo.

## Follow-up: tier 2 redesigned (same day)

Tier 2 now judges a comment only when the change touched something it names, asks only
whether the after-code contradicts it, never edits, and has no "possibly stale" verdict
(`docs/plan-phase4.md`, "Review notes after the 30-PR validation").

Re-scored offline on the same 83 findings, from the before and after blocks the run stored:

| | Before | After the gate |
|---|---|---|
| Tier-2 items sent to the judge | 83 | 1 |
| Of the 79 false findings | 79 | 1 |
| Of the 4 not-false findings | 4 | 0 |

The 1 candidate is `fold_constants.rs:359` ("Simplified version of `tryFoldAdd`"), whose
function's signature changed. The count is the same whether the gate reads only the annotated
code or also the enclosing scope, as it now does. Its g1 from the old run was 0.04, so it would be kept unless the
new question scores it above 0.5 (not measured; the judge server was not run). 77 of the 83
had identical before and after blocks: only the enclosing function changed.


## Re-run after the tier-2 redesign (2026-10-19)

### Verdict

- **The Phase 4 exit criterion is met on these 30 PRs.** Tier 2 produced 0 findings, so it made
  no false deletes and no false stale calls.
- **The cost is tier-2 recall.** On 30 planted stale comments it caught 3 (10%). On 30 planted
  still-correct comments it flagged 0.
- **Tier 1 deletes the wrong comment about 1 time in 18.** Of all 500 tier-1 deletes, 28 (5.6%)
  remove a comment worth keeping. 4 (0.8%) remove a warning a reader needs.

### What ran

- All 30 PRs finished, report-only, with the merged branch and the locally fine-tuned
  Clef-Flash judge (retrained with verification rows). Every worktree was clean afterwards.
- Total runtime was 2,111 seconds. async-code #2 had 1 check error (one file that would not
  parse, as before).
- Findings: 500 tier-1 delete, 2 tier-1 review, 1 tier-1 reduce, 0 tier-2.

### Tier-2 recall on planted changes

60 mutation pairs were built from these repos: 30 where a change makes a comment wrong, 30
where a nearby change leaves it correct.

| | Stale (30) | Still correct (30) |
|---|---|---|
| Passed the gate | 15 | 1 |
| Judge said stale | 3 | 0 |

- 15 stale cases never reached the judge: 3 had no item for the comment, 12 were not flagged
  by the gate.
- The judge rejected the other 12. Only reflex (0.448 against a 0.5 threshold) was close.
- So the gate and the judge each lose about half of the real stale comments.

### Tier-1 hand audit

All 500 tier-1 deletes were read by hand with about 10 lines of context at the PR head. No
code was run, and no upstream callers or docs were checked.

| Result | Count |
|---|---|
| Safe to delete (restates the code, a section label, or a test step) | 472 |
| Should be kept (rationale or contract) | 24 |
| Should be kept, and deleting loses a warning | 4 |

The 4 that lose a warning:

- TanStack/db #198, `collection-lifecycle.test.ts:239`: `gcTime: 5000, // 5 seconds` is the
  only place that says the value is in milliseconds.
- Wei-Shaw/claude-relay-service #167, `unifiedGeminiScheduler.js:148`: says a lower priority
  number ranks first, which the sort alone does not show.
- Wei-Shaw/claude-relay-service #167, `unifiedGeminiScheduler.js:204`: says the bare `3600` is
  a one-hour session lifetime in seconds.
- OpenAgentsInc/openagents #1257, `React19Icons.tsx:23`: says the icon casts exist for React 19
  compatibility. Without it the casts look removable.

The 24 rationale losses are mild. They cluster in two PRs: claude-relay-service #167 (notes
that requests mirror gemini-cli's format, which upstream logic to keep in sync, and what an
unnamed argument means) and TanStack/db #198 (why a test restarts sync or emits after start).
Others: memfs #1144 (`glob.ts:140`, the only sign an empty catch is deliberate), AzureTRE
#4608 (a test filter copied from production code), dot-ai #50 (why a candidate limit is
doubled), zcf #19 (a backward-compatibility re-export), datafusion #17520 (`SELECT FROM`
returns rows with no columns).

### What this means

- Tier 2 is now safe but nearly silent. It will rarely catch a stale comment.
- Tier 1 is mostly right. Its wrong deletes are comments that carry a unit, an ordering rule,
  or a link to an outside format. Those are the cases to teach it next.

### Not checked

- Each audit was one reading by one checker. Borderline calls were judged strictly and could go
  either way.
- Why the gate skipped each of the 15 missed stale cases, beyond "no item" or "not flagged".

Raw data stays outside the repo: per-PR summaries, findings, the audit files and the
mutation results.
