# Ship-scoped skill learning: findings for #453

Research snapshot: 2026-10-28. Privateer base: `c31539bb7ec3f5c526f8339f69941a4900f2e38c`.
Hermes source: [`0c74cc682dc81bf78b78a1737b3ee77a820ccb79`](https://github.com/NousResearch/hermes-agent/commit/0c74cc682dc81bf78b78a1737b3ee77a820ccb79), committed 2026-10-28.
This is a findings report for [#453](https://github.com/kirkins/privateer/issues/453), not an agreed implementation design.

**Recommendation:** keep capture manual now; evaluate a ship-local **Suggested** mode
that records evidence-backed candidates at normal task handoffs. Reuse notes,
task custody and the approved bundle registry before considering new storage.
Do not enable automatic post-turn learning. Static accounting shows why smaller
review inputs deserve evaluation; it does not show that they produce good skills.
No live skills, prompts, settings, services or routes were changed for this study.

## What the source actually does

The linked source is authoritative for this snapshot. Current website pages are
useful guides but can drift. Nothing here is a live Hermes deployment measurement.

| Capability | Source-confirmed behavior | Implication for Privateer |
| --- | --- | --- |
| Manual learning | [`build_learn_prompt`][h-learn] turns `/learn` into a normal agent turn. The live agent gathers sources and authors through `skill_manage`; large sources use a small index with references. Authoring standards alone measure 1,120 tokens in both benchmark encodings. | An explicit capture request can use the existing working session. Do not invent a separate distillation service first. |
| Skill writes | [`skill_manage`][h-manage] supports create, patch/full rewrite, supporting files, delete and atomic operation batches. A per-skill lock surrounds read/modify/write; guarded writes validate and scan, reverting a blocked change. The scanner is a heuristic, not proof of safety. | Draft outside the selected/pinned tree. A valid file is not automatically approved content or executable authority. |
| Write approval | [`write_approval`][h-approval] defaults off. When enabled for skills, writes stage for review regardless of foreground/background origin; pending diffs and explicit replay are available. The manager's import-error path permits writes if the approval module cannot load. | Reuse the idea of staged diffs, not the default or failure policy. Content review and production deployment approval remain separate. |
| Eligible post-turn review | [`turn_finalizer`][h-finalizer] checks the skill iteration threshold and availability of `skill_manage`; a nonempty, uninterrupted final response and enabled review path are required. It resets the counter when the threshold fires. Memory can trigger review separately. | “After every turn” in a module description is not an unconditional extra call on every turn. Compare eligible reviews explicitly. |
| Defaults differ | [`agent_init`][h-init] uses a fallback skill interval of **10**; [`cli-config.yaml.example`][h-config] specifies **15** and memory nudges of 10 user turns. | The resolved installation config determines cadence. A source fallback and a sample config are different facts. |
| Review scheduling | [`background_review`][h-background] defaults automatic review on, except safe-worker/finite-turn exclusions. It uses a daemon thread, one admitted review token per parent, live-turn cancellation and a bounded acknowledgment wait. [`review_idle_queue`][h-idle] can coalesce snapshots for a managed local runtime; it is an in-memory best-effort queue with an age limit. | Reuse foreground priority and coalescing; a thread or local idle slot is not durable fleet custody or a daily spend ledger. |
| Review context | Same-model forks copy the cached system prompt, tool schemas and cache identity. A different-model route instead gets a digest plus recent messages. Dispatch restricts tools even when the advertised schema remains broad for cache parity. Routing failure can fall back to the parent model. | A narrow prompt alone does not restrict tools. Privateer must refuse an unauthorized route, not quietly spend on a fallback. |
| Review limits and usage | The fork has a 16-iteration limit and an aggregate input-token threshold derived from 75% of context, capped at 600,000 (unknown-context fallback 120,000); nonpositive explicit configuration disables that threshold. Usage is attributed to the parent, including error/cancellation paths. These are request-loop controls, not a ship/day monetary reservation. | Count outputs, retries, cancellation and tool-loop replay. Pre-call reservation and unknown-usage handling are necessary before claiming a hard daily cap. |
| Write guards | [`skill_manager_guards`][h-guards] tracks fresh reads before overwriting existing skill files during review. Autonomous delete paths have provenance/pinning/consolidation restrictions. Existing-file reads do not make imported instructions trustworthy. | Check the exact base pin again when approving a diff. Preserve a conflicting candidate for review instead of overwriting another author's update. |
| Provenance and recovery | [`skill_ledger`][h-ledger] stores mutation records and content-addressed before/after blobs, supports rollback and bounded ledger retention. Recording is best effort rather than a write gate; rollback requires a safety capture. [`skill_usage`][h-usage] records creation/use/patch provenance. | Useful audit and rollback patterns; not proof that every mutation has a durable complete audit trail or that a skill helped. |
| Progressive discovery | [`prompt_builder`][h-prompt] renders a descriptor index, including names-only categories; [`skills_tool`][h-skills] lists metadata and loads bodies/linked files on demand. Readiness/platform/project/profile filters affect visibility. [`skills_index_delta`][h-delta] announces changes after a frozen prompt without rebuilding its prefix. | Bound discovery as well as bodies. Do not advertise all fleet skills to every ship or silently change a parked job's selected plan. |

The skill-review prompt encourages frequent updates and prefers modifying an
existing class-level skill. That is a policy choice in Hermes, not evidence of
learning quality. Its **1,925-token** literal skill-review instruction also has a
meaningful footprint before history, schemas or tool responses. Do not copy that
pressure to produce a change: “no durable lesson” should be a successful review.

Not established by this audit: actual review frequency, billed savings/cache hit
rates, learning accuracy, secret-scan recall, or the effectiveness of curator
deduplication. Matching names, fresh reads and provenance cannot prove semantic
equivalence. No upstream code was imported, run or copied into Privateer.

## Choose the cheapest useful capture trigger

| Approach | Additional model work | Useful signal / weakness | Recommendation |
| --- | --- | --- | --- |
| Manual capture | Only when requested; source gathering and drafting can still be expensive | Clear intent; easy to forget | Available workflow now; baseline for evaluation |
| First-mate suggestion at an existing handoff | A short conditional suggestion in work already being done; drafting is a separate choice | Uses the reasoning already in context; may over-suggest | Preferred first evaluation; no constant reminder |
| Deterministic event candidate | No model call to count typed events; one review only for selected candidates | Repeated error code plus verified recovery, repeated failed test, explicit captain correction linked to an outcome | Evaluate on synthetic records; store evidence pointers, not transcripts |
| Idle or scheduled review | An extra context plus possible replay, even when no candidate is useful | Can see cross-task patterns; idle CPU says nothing about model budget | Later opt-in only, bounded review window and capped queue; no periodic paid inference by default |
| Eligible post-turn fork | Separate model loop after threshold-triggering work | Fresh context is available; substantial replay and concurrency cost | Comparison baseline, not the proposed default |

A repeated failure alone is often a defect to fix, not a skill. A deterministic
detector can count normalized error types, failed attempts and verified recovery
records; it cannot infer that a natural-language correction is authoritative or
that the procedure generalizes. Explicitly tagged corrections are stronger than
guessing intent from sentiment. Group by stable task/outcome plus failure class,
coalesce evidence, and suppress a rejected or already captured lesson until its
evidence changes. Surface at most one suggestion at a normal completion/handoff;
do not wake a ship just to ask whether it learned something.

An extra model review earns its cost only if it discovers a repeatable procedure
missing from an existing skill or documentation, with a falsifiable verification.
Otherwise retain a short note, link the existing document, or discard the candidate.

## Scope and custody

| Scope | Intended use | Visibility / inheritance rule to evaluate |
| --- | --- | --- |
| Ship | Durable procedures for the ship's charter | Default candidate home; one recorded ship owner |
| First mate | Coordination procedure | Explicit first-mate selection; not implicitly copied to crew |
| Task or project | Repo conventions or an incident investigation | Prefer repo docs/task notes; expire with the task unless deliberately promoted |
| Crew job | Minimal resources needed for this assignment | Explicit immutable selection at launch; descendants independently select approved refs |
| Machine or home | OS facts, endpoints, model preferences, credential references | Private configuration or optional plugin; never put these into a general reusable skill |
| Fleet curated | Procedure demonstrably useful across charters | Explicit reviewed share/import, recipient opt-in and a version pin |

Use a stable origin ship/task/candidate identity and content digest, never a
hostname or absolute path as ownership. A moved ship keeps its identity and
candidate custody; its new machine separately validates local paths, approved
registry and availability. Duplicate mail after a move must not create a second
candidate or spend reservation. A peer recommendation is evidence only: it cannot
change the recipient's selection, permission policy or outcome owner. A parent
may recommend refs to a descendant, but current Privateer does not inherit them.

## Existing Privateer seams and their limits

| Existing seam | What can be reused | Constraint that learning must preserve |
| --- | --- | --- |
| [`src/context.rs`](../../src/context.rs) | Versioned, bounded `note.put`, exact `note.pin`, task/focus context | Notes retain only recent versions; pins are not an unbounded archive. Candidate evidence can link records, but lifecycle/spend invariants cannot be assumed from a note. |
| [`src/tool_bundles.rs`](../../src/tool_bundles.rs) | Operator-only `bundle.configure`, transactional registry updates, immutable refs/history, plan digest | Agent content approval must not become registry-write authority. Removal revokes future selection; reusing an old ref for different content is refused. |
| [`plugins/bundles.py`](../../plugins/bundles.py) | Shared pin/path/frontmatter validation and selected plan protocol | At most 8 skills/bundle, **16 skills/plan**, 32 bundles/registry; registry and plan byte caps. Trees are bounded to 128 entries/2 MiB. These are not token budgets. |
| [`src/profile.rs`](../../src/profile.rs) | Names/descriptions for selected skill inventory | Metadata is not the whole body. `skill_description` expects LF frontmatter while the Python validator accepts CRLF: a known inventory follow-up, not a reason to redesign storage. |
| [`plugins/pi_bundles.py`](../../plugins/pi_bundles.py) | Explicit flags disable ambient discovery, capability preflight, approved paths only | Bounded Pi resumes/forks are refused. Legacy omitted selection remains ambient; `[]` explicitly requests bounded empty mode and still needs an approved registry. |
| [`plugins/claude_bundles.py`](../../plugins/claude_bundles.py) | Selected plugin roots, strict MCP config, reviewed carried settings | Project settings/skills and built-ins still load. Claude and Pi have different exposure guarantees; neither is an OS sandbox. |
| [`src/activation.rs`](../../src/activation.rs), [`docs/tool-bundles.md`](../tool-bundles.md) | Plan identity recorded at launch; supported Claude resume tied to the same digest and role/task lineage | A changed plan cannot silently hot-replace a parked crew's context. First-mate/crew selections are separate. Other harnesses are not qualified for these bundle guarantees. |
| [`src/usage.rs`](../../src/usage.rs), [`src/jobs.rs`](../../src/jobs.rs) | Settled job usage grouped by route/ship/day/machine, cache fields and explicit gaps/partial logs | Historical accounting is not pre-call admission. Unknown/missing usage is not zero. Hidden harness-side forks would evade any claim of complete Privateer learning attribution. |

Existing provider route and resource admission still apply to any future review
job. Route fallback to another harness can drop bundle configuration; a learning
job requiring bounded skills must refuse that fallback unless the actual target
has the qualified plan. Local storage/launch pins detect drift at launch, not
same-user concurrent mutation or unauthorized shell reads after launch.

**No new skill database is justified yet.** For the first offline experiment use
synthetic candidate JSON plus ordinary reviewed repository diffs. An eventual
durable candidate queue would need separately reviewed transactional deduplication,
custody, retention and budget reservation behavior; notes/mail alone do not prove it.
Approved drafts should become a new version in the existing pinned bundle registry.
Do not edit the currently selected tree in place or add unsupported frontmatter:
Privateer's narrow format accepts only `name` and `description`; Hermes's richer
metadata requires an explicit conversion/review, with provenance kept outside it.

## Measured text footprints and explicit assumptions

The [measurement script](../../scripts/research/skill_learning_costs.py) generates
only synthetic English fixtures. [Checked-in results](skill-learning-costs.json)
use `tiktoken==0.14.0`, `o200k_base` and `cl100k_base`. These are exact counts for
those encodings and strings, **not actual provider billing** or estimates of every
harness's tokenizer. Native descriptor wrappers can differ. Upstream prompt text
is parsed as static literals from hash-checked files, never executed or vendored.

| Input | o200k tokens | cl100k tokens | Meaning |
| --- | ---: | ---: | --- |
| 8 short descriptors | 122 | 122 | Small selected catalog |
| 16 short descriptors | 234 | 236 | Current Privateer plan ceiling, short descriptions |
| 40 / 160 short descriptors | 570 / 2,250 | 578 / 2,288 | Hypothetical broader catalogs; **not valid bounded Privateer selections** |
| 16 descriptions at 1,024 characters | 2,767 | 2,767 | Character limits alone permit substantial catalog growth |
| One synthetic body | 469 | 469 | Fetched on demand; linked references would cost more |
| Synthetic conditional nudge | 22 | 22 | Per injection; repeated reminders also remain in history |
| Synthetic candidate packet | 151 | 150 | Bounded evidence/lesson metadata; source retrieval extra |
| 20-turn synthetic history | 4,360 | 4,360 | Short generated turns and tool records, not a live trace |
| Literal Hermes memory / skill / combined review instruction | 210 / 1,925 / 1,769 | 212 / 1,932 / 1,778 | Instruction only; excludes all inherited context |

Chosen workload: **8 ships, 20 completed turns/ship/day, 16 short descriptors,
4 eligible reviews/ship/day** after synthetic turns 5/10/15/20. Compare with **one
event review/ship/day**. Eligibility frequency, candidate usefulness and throughput
are scenario assumptions, not observations of either product. Each review here
is one first request: no full system prompt, schemas, message framing, generation,
tool loop, source fetches, retries, tests, compaction or periodic jobs.

| Fleet/day text input | o200k tokens | Interpretation |
| --- | ---: | --- |
| Foreground selected catalog | 37,440 | Context traffic, not necessarily all uncached billing |
| Extra 22-token nudge on every turn | 3,520 | Immediate added text only; later replay is excluded |
| Two body fetches/ship | 7,504 | Payload once per fetch; later transcript replay excluded |
| 32 eligible full-history reviews, short neutral instruction | 97,184 | Isolates replay/call-frequency effects without Hermes's large instruction |
| Same replay with literal Hermes skill-review instruction | 156,288 | A Hermes-shaped partial-input scenario, not a complete Hermes request |
| 8 candidate reviews, short neutral instruction | 3,704 | Smaller context and fewer calls; **quality may be worse** |
| Review every turn, short instruction (stress case) | 416,160 | Deliberate stress workload, not Hermes's default |

The neutral-instruction comparison is about **26 times** less first-request text
for event review in this chosen workload. It changes both call count and evidence
coverage. It is not a measured 26-times reduction in cost, latency or useful
learning. A small packet can omit the decisive failure; retrieval and validation
can erase its apparent advantage. A scheduled review that replays the same history
adds the same kind of expense even when it runs while the machine is idle.

For actual accounting, sum each request's disjoint categories:

```text
cost = (uncached_input * rate_in + cached_read * rate_read
        + cache_write * rate_write + output * rate_out) / 1_000_000
daily_cost = sum(review + retrieval + draft + retry + test_model_calls + periodic)
concurrent_context = sum(active request contexts), not just the largest request
```

Providers report cache categories differently; normalize without double counting
before applying configured route rates. Track input/output and cache separately,
including no-change, rejected, failed and cancelled reviews. Stable prefixes can
make full replay cheaper than its raw count suggests; model/route changes, expiry,
prompt edits and changed tool schemas can make it cold. Cross-ship cache sharing
and cache hits are not assumed. Offline counts cannot establish either outcome.
Generation, tool/test work and a periodic scan need their own measured allowances;
this script reports no model-output or dollar figure because no model was called.
The 469-token synthetic body can also serve as a *chosen output-size fixture* for
drafting; it is not an observed generation. A same-model digest variant and
end-to-end drafting/test loops remain unmeasured. Evaluate them alongside actual
retrieval before claiming a complete cost comparison. Local models replace some
API billing with RAM/GPU contention, power and latency; “idle” does not make that
capacity free or a small machine suitable for another resident model.

A proposed cap must reserve a conservative maximum before each dispatch under the
same transaction as admission; concurrent jobs cannot each spend the same balance.
Reconcile reported usage after completion, retain the reservation if usage is
unknown, and never refund merely because a wrapper exited. Refuse further requests
at the per-review and ship/day ceiling. A kill switch stops new admission and
requests cancellation of in-flight work; verify payload completion before calling
it stopped. Current Privateer usage summaries alone do not implement these rules.

### Reproduce the accounting

Use an isolated environment, with no live configuration or credentials:

```bash
uv venv /tmp/privateer-skill-costs-venv
uv pip install --python /tmp/privateer-skill-costs-venv/bin/python tiktoken==0.14.0
/tmp/privateer-skill-costs-venv/bin/python scripts/research/skill_learning_costs.py
```

The first encoding load can fetch public tokenizer data. Subsequent cached runs
need no network. To reproduce upstream prompt rows, fetch only
`agent/background_review.py` and `agent/learn_prompt.py` from the pinned Hermes
commit into a separate directory preserving these relative paths, then pass
`--hermes-source DIRECTORY`. The script rejects different bytes using the SHA-256
values in the results. Without that option, synthetic results are identical and
upstream rows are deliberately absent. No runtime module is imported.

## Candidate lifecycle and captain controls to evaluate

Proposed workflow, not implemented:

```text
typed signal -> ship-owned candidate -> review or reject -> draft diff
             -> fixture verification -> reviewed new bundle ref
             -> explicit role/ship selection -> observed outcomes
```

A candidate needs stable ID, owner, scope, source task/job/event IDs, evidence
digests, lesson and counterexample, proposed target/base pin, expected benefit,
verification and expiry. Keep large evidence in the source records and retrieve
it deliberately. Do not copy tool output, mail or repo instructions into trusted
policy. A captain correction can justify content, but quoted “captain says” in a
tool result cannot authorize anything. Record origin/trust and review it explicitly.

Prefer a task note for transient state; a ship brief for current coordination;
memory/private config for personal facts; repo docs for a project procedure;
retrieval for bulky factual reference; a tool bundle for approved executable
capability. Use a skill for a reusable procedure with a concrete trigger, limits
and check. A short note linked to existing docs is often enough.

Before approving, compare existing names **and meanings**; merge duplicates with
evidence pointers, separate conflicting lessons, and reject single-incident
overgeneralizations. Review scripts/dependencies and secret leakage separately.
No permission, shell access, credential reference or model route is widened by
content approval. Bind approval to the exact diff and base digest; changed bases
require re-review. Keep previous pinned versions and verification evidence for
rollback; expire stale candidates and mark obsolete approved versions unavailable
for new selection. Production deployment still needs the captain's exact-artifact,
environment-bound approval or applicable standing grant, and a deployment record.

| Mode | Capture / generation | Control surface required before any rollout |
| --- | --- | --- |
| Manual | Explicit request, ordinary reviewed diff | Existing owner and content review; separate bundle configuration/deployment authority |
| Suggested (evaluation default) | Conditional suggestion at handoff; candidates only; review/draft on selection | Per-ship enable/disable, one visible suggestion, dedup/cooldown, dismiss/edit/reject, bounded retention |
| Automatic (not recommended yet) | Explicitly granted background candidate review or drafting | Per-review and ship/day token/spend caps, time window, concurrency cap, route allowlist, fail-closed unknown usage, audit and kill switch |

Suggestion permission is not background-generation permission. Automatic drafting
is not automatic registry publication. Display queue age, proposed/approved status,
evidence, rejection reason, actual/gap usage and expiry in any later control surface.
Failure should leave a reviewable candidate with its owner, never silently retry
forever or promote an untested skill. Retention and spend policy belong in optional
private configuration; the runtime should not embed fleet-specific defaults.

## Smallest next experiment and stop/go decision

**Proceed only to a separately scoped, non-production quality evaluation.** The
accounting supports testing Suggested mode; it does not justify a learning daemon.
This report opens no implementation tickets and enables no background inference.

1. Use 24 synthetic cases: six real reusable recoveries, six transient facts or
   existing-doc answers, six duplicates/conflicts, and six injection/secret/authority
   traps. Human-label expected destination and evidence before running variants.
2. Compare manual capture, a suggestion in the existing handoff, and event-packet
   review with an eligible full-replay baseline on exactly the same cases. Freeze
   model/route, prompts and fixtures. Evaluate outputs blind against held-out
   counterexamples, not whether the model wrote a skill. Reserve any paid evaluation
   budget explicitly first; no model calls were made by this research script.
   A starting event-review envelope to evaluate is 8,000 aggregate input tokens,
   1,000 output tokens, at most two requests, one review/ship/day and one concurrent
   review fleet-wide. Reserve the full envelope at configured route rates, including
   possible cache writes; the captain sets the monetary ceiling. The replay baseline
   needs its own declared envelope. These are experiment limits, not live defaults.
3. Record useful/rejected/no-change results, missed lessons, false suggestions,
   source retrieval, complete input/output/cache usage, latency, human review time
   and successful recurrence handling. Test rollback and stale-base refusal using
   disposable directories only. Benchmark executable suggestions only in a sandbox.
4. Stop on any secret exposure, unauthorized action, custody loss, duplicate review
   after restart, unbounded queue/retry, unknown unreserved spend, or changed-base
   approval. For a later queue prototype, require crash/restart, duplicate delivery,
   concurrent cap reservation, move and stale acknowledgment fixtures before live use.
5. Proposed go criteria: capture at least five of six reusable lessons, no more than
   one false suggestion among the six transient cases, no authority violations,
   correct dedup/conflict handling and verified rollback. Suggested/event review must
   beat full replay's total cost **and** preserve quality on these cases. Thresholds
   are an experiment proposal, not a statistically validated production standard.

Captain decisions after reviewing the findings: whether to run this small quality
evaluation; its exact model route and total budget; acceptable missed-lesson and
review burden; and whether any successful candidate warrants a separate scoped
implementation proposal. Until then, explicit capture and selective pinned skills
remain sufficient. Long-term autonomous benefit and cross-harness safety are open.

[h-learn]: https://github.com/NousResearch/hermes-agent/blob/0c74cc682dc81bf78b78a1737b3ee77a820ccb79/agent/learn_prompt.py
[h-manage]: https://github.com/NousResearch/hermes-agent/blob/0c74cc682dc81bf78b78a1737b3ee77a820ccb79/tools/skill_manager_tool.py
[h-approval]: https://github.com/NousResearch/hermes-agent/blob/0c74cc682dc81bf78b78a1737b3ee77a820ccb79/tools/write_approval.py
[h-finalizer]: https://github.com/NousResearch/hermes-agent/blob/0c74cc682dc81bf78b78a1737b3ee77a820ccb79/agent/turn_finalizer.py
[h-init]: https://github.com/NousResearch/hermes-agent/blob/0c74cc682dc81bf78b78a1737b3ee77a820ccb79/agent/agent_init.py
[h-config]: https://github.com/NousResearch/hermes-agent/blob/0c74cc682dc81bf78b78a1737b3ee77a820ccb79/cli-config.yaml.example
[h-background]: https://github.com/NousResearch/hermes-agent/blob/0c74cc682dc81bf78b78a1737b3ee77a820ccb79/agent/background_review.py
[h-idle]: https://github.com/NousResearch/hermes-agent/blob/0c74cc682dc81bf78b78a1737b3ee77a820ccb79/agent/review_idle_queue.py
[h-guards]: https://github.com/NousResearch/hermes-agent/blob/0c74cc682dc81bf78b78a1737b3ee77a820ccb79/tools/skill_manager_guards.py
[h-ledger]: https://github.com/NousResearch/hermes-agent/blob/0c74cc682dc81bf78b78a1737b3ee77a820ccb79/tools/skill_ledger.py
[h-usage]: https://github.com/NousResearch/hermes-agent/blob/0c74cc682dc81bf78b78a1737b3ee77a820ccb79/tools/skill_usage.py
[h-prompt]: https://github.com/NousResearch/hermes-agent/blob/0c74cc682dc81bf78b78a1737b3ee77a820ccb79/agent/prompt_builder.py
[h-skills]: https://github.com/NousResearch/hermes-agent/blob/0c74cc682dc81bf78b78a1737b3ee77a820ccb79/tools/skills_tool.py
[h-delta]: https://github.com/NousResearch/hermes-agent/blob/0c74cc682dc81bf78b78a1737b3ee77a820ccb79/agent/skills_index_delta.py
