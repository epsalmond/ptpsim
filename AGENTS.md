

This file is the project-specific override layer for *all* CLI agents. Read any
user- or tool-level instructions supplied by your environment too; this file
takes precedence on anything ptpsim-specific.

If `.local/AGENTS.md` exists and is readable, read it after this file.

## What ptpsim is

A scriptable, OSS camera-protocol simulator (PTP/IP responder). One generic
engine, no per-manufacturer code; cameras are added as manifest **data**
(`packages/camera-config-data/`) authored from probe evidence through the
in-repo generator intake (see DESIGN.md "Operating model"; the predecessor
standalone mapper toolkit is archived). Dual-licensed MIT OR Apache-2.0.

Full design: [`DESIGN.md`](DESIGN.md). Consumer-facing FFI surface and
verb grammar: [`docs/INTEGRATION.md`](docs/INTEGRATION.md). Manifest
schema authority: [`docs/MANIFEST_SCHEMA.md`](docs/MANIFEST_SCHEMA.md)
(the "contract tiebreaker" — when the code and other docs disagree,
it wins; its §11.x numbering is historical).

## Working conventions

### Open-source hygiene — nothing private lands here

This is a public repo. Repo content — docs, code comments, CI config, fixtures —
must stand alone for an outside contributor with no access to the authors'
machines. Never add:

- Private hostnames, IPs, or registry/CI endpoints (internal registries,
  tailnet names, and dev-machine hostnames).
- Absolute or user-specific paths into private checkouts or trees.
- Load-bearing cross-links into private issue trackers (`<consumer-repo>#123`).
  Restate the durable fact in place. A public issue may include one neutral
  supporting-analysis link only when the issue remains complete without it;
  tracked repo artifacts never depend on that link.
- A private consumer's internals (app source paths, backend/service topology).
  Naming a consumer as a motivating example is fine where load-bearing; its
  implementation details are not.

The test: would the line make sense on a stranger's laptop? If not, it belongs
in the consumer's repo or a private tree. Existing violations are tracked
(#255, #160, #259) — do not add new ones while those burn down.

### Prose register

Write short declarative sentences that state facts. Titles state the content
("Vendor manifest schema"), never slogans. Banned copywriting tics: epigram
headlines ("One protocol, many cameras"), antithesis frames ("It's not X,
it's Y"), tricolons ("small services, one bus, one truth"), and mirror-clause
endings (a closing line that echoes the opening line). Avoid em dashes: use
commas, colons, parentheses, or two sentences. The antithesis ban is on the
rhetorical slogan shape; ordinary contrast that states a fact ("this measures
X, not Y") is fine, and so are words that record real design intent
("deliberately"). Test: if a sentence would fit on a landing page, rewrite it
as a fact. Applies to new and edited prose in docs, PR bodies, issues, and
code comments. Clean up an existing violation when editing that text anyway;
do not sweep-rewrite otherwise-untouched prose to conform.

### Work tracking — GitHub issues, not in-process task lists

**Work is tracked as GitHub issues.** Agent in-process task lists

model swaps, and the user does swap between models and agents. Anything worth
remembering across sessions goes in `gh` so it's discoverable from
`gh issue list`.

This means: when you surface a follow-up, a deferred fix, or a "we should do X
later" — file an issue. An actionable finding against an open pull request is
tracked first as a durable PR review thread under the workflow below; if it is
deferred from that PR, create an issue and link it before resolving the thread.
The in-process list is for *this turn's* working memory, not the project
backlog.

**Priority labels:** `priority:P1` — on the active release or program critical
path, pick these up first; `priority:P2` — consumers hit it soon, schedule next;
`ready` — implementable without new protocol evidence;
`needs:protocol-evidence` — implementation is blocked on protocol or camera
behavior evidence; unlabeled — backlog. `ready` and
`needs:protocol-evidence` are mutually exclusive on one implementation issue.
Split nonblocking unknowns into their own evidence issue instead of carrying
both labels.

### SDLC — worktree → branch → PR, review, merge

1. **One agent session per worktree**, so sessions don't collide:
   `git worktree add ~/git/ptpsim-<slug> -b <branch>` off `main`. It's
   yours alone — multiple branches/PRs in it are fine, never a shared
   checkout. `git worktree remove` when done. Do this **before your first
   edit**, not at commit time — never work in `~/git/ptpsim` itself.
   Enforced by `scripts/git-hooks/pre-commit` (blocks commits on `main`
   and in the primary checkout); activate once per clone with
   `git config core.hooksPath scripts/git-hooks`.
2. **Close issues with Pull Requests.** An issue SHOULD precede the PR (see
   *Work tracking*); footer the PR `Closes #N`. A self-evident doc fix may
   skip the issue.
3. **Delegate implementation to Codex (GPT-5.6 Sol) subagents at high
   effort, regardless of orchestrating harness, unless explicitly
   overridden.** Open a draft PR from
   the first commit; the draft is the work's visible home while in flight, so
   don't wait for polish to push. Before marking it ready: run the workspace
   checks (*Build + test*), then run exactly one independent `/code-review`

   review-thread workflow below.
4. **Single-line commit messages.** Imperative (`Add logging for X`), no
   body. The *why* goes in the PR description — or a durable `docs/*.md` if
   it's load-bearing past the merge. Commits should not have "and."
5. **Push the branch, mark the PR ready, then merge it directly.** GitHub
   auto-merge is unavailable on this repository (free-plan private repo with no
   branch protection). The owning agent merges only after the review-complete
   gates below and required checks pass on the current head; do not bypass
   those gates.
6. **Write the PR for a human:** what was done and why it matters, in prose
   a reviewer follows. No opcode/method dumps or
   change-by-change logs — reviewers can read the code for that. Shorter is easier to
   reason about.
7. **Never** force-push, `git reset --hard` shared refs, amend published
   commits, or use `--no-verify`. A failing hook is signal — fix the cause.

Documentation SHOULD precede the code change, not follow it.

Run the workspace checks (*Build + test*) before pushing.

### Code review — durable PR threads, bounded fixes

Review state lives on the pull request, not in agent chat or an in-process
task list; a finding that exists only in chat does not count as review state.
The full workflow — pinning the review to a SHA, durable thread triage,
narrow fix delegation, delta reviews, and the readiness gates that end in the
owning agent merging its own PR — is
[`docs/CODE_REVIEW_WORKFLOW.md`](docs/CODE_REVIEW_WORKFLOW.md). Read it
before running or remediating a `/code-review` pass, and when resuming a PR
after context loss (resume from PR state, not chat).

### Prose register

Docs, PR bodies, and issues are reference material: terse, concrete,
declarative. Titles state the content ("Manifest schema changes"), never
slogans. Avoid copywriting tics: epigram headlines, antithesis frames ("it's
not X, it's Y"), tricolons ("one engine, no forks, all data"), and
mirror-clause endings. Avoid em dashes: use commas, colons, parentheses, or
two sentences. If a sentence would fit on a landing page, rewrite it as a
fact. Applies to new and edited prose: existing text predates this rule, so
fix it when you touch it rather than sweeping.

### Private evidence and operator-only tooling

This public repository must remain usable without access to private hosts,
operator trees, captures, or issue trackers. Search the repository and its
public issues before requesting evidence that is not available here.

If your environment separately provides a private evidence or operator system,
follow that system's own current instructions. Do not copy its hostnames,

narrative into this repository. Bring back only the scoped conclusion and the
public-safe evidence needed to review the manifest, schema, test, or design
change.

When required evidence is unavailable, file an evidence-gated public issue that
states the literal question, camera/body and firmware scope, existing public
anchors, safety constraints, and completion condition. Do not guess protocol
behavior or present private access as a prerequisite for outside contributors.

The public GitOps lifecycle is:

1. A consumer or contributor files the need in ptpsim. A maintainer makes the
   literal evidence question and scope explicit using the public template in
   `docs/consults/README.md`, then applies `needs:protocol-evidence`.
2. An external evidence system may discover the labeled issue and investigate
   under its own rules. It does not edit any tracked ptpsim artifact, including
   code, manifests, fixtures, tests, or documentation. Its only handoff into
   ptpsim is the self-contained issue response for normal contributor review
   and integration.
3. The ptpsim issue receives a self-contained answer: applicable scope,
   established behavior, uncertainty or falsifier, and the implementation or
   public-fixture consequence. A neutral supporting-analysis link is optional
   and never load-bearing.
4. A ptpsim maintainer reconciles the result: an answer that makes
   implementation possible removes `needs:protocol-evidence` and adds `ready`;
   a sufficient negative answer resolves or re-scopes the issue without
   `ready`; an inconclusive answer retains `needs:protocol-evidence` and states
   the smallest missing observation.

Never expose an evidence provider's internal role names, commands, host paths,
raw-capture locations, fact identifiers, or analysis narrative in this repo.
Promote only the scoped conclusion and public-safe reduced evidence. GitHub
issues are the resumable queue; agent memory or a private orchestration session
is not lifecycle authority.

The existing `docs/consults/` flow remains the public consumer-contract path.
Private evidence systems may inform that work, but do not replace its durable,
public review trail.

### Schema and manifest changes

ptpsim is **pre-production** (no third-party consumers locked to the
current grammar). When choosing between a minimum-change workaround
and a small schema cleanup, **prefer the cleanup** if it would
otherwise carry a name/semantic mismatch into data files. Don't
accumulate semantic debt for future agents to inherit. The
`bleNotify` → `bleSubscribe + bleNotify` split (2026-06-09) is the
canonical example.

Memory note expanding on this: `feedback-eager-schema-cleanup-preproduction`.

### Where things live, where things don't

- **Manifest data** lives in `packages/camera-config-data/`. Authored by
  people, proposed from `camera-observation/v1` runs, and reviewed before
  merge. Agents don't hand-author YAML manifests — they can edit small curated
  entries (gatt catalog additions, establishment-step ordering), but bulk data
  flows through `camera-config-generate propose` → digest-bound review →
  `camera-config-generate apply`. The archived standalone mapper is historical
  provenance only. Memory:
  `feedback-data-via-generator-not-agent-authoring`.

  do NOT belong in ptpsim source.** Private analyses stay in their owning
  systems and must not be referenced by private path. Promote only the
  public-safe evidence, reduced fixture, or scoped conclusion needed to review
  the manifest/code change. Memory: `feedback-no-operations-in-ptpsim-source`.
- **FFI types are hand-written**, not derived. Every `camera-config`
  schema add needs a matching FFI variant (in
  `crates/camera-protocol-ffi/src/mfg_index.rs`'s `Step` /
  `ScanObservation` / etc. enums) + a `From` arm + a seam test, or the
  schema silently doesn't reach consumers. Memory:
  `feedback-ffi-mirrors-camera-config-manually`.
- **One generic engine, never per-brand crates.** No `crates/fuji-*`,
  no `crates/nikon-*`. Vendor specifics live in manifest data; engine
  primitives in `crates/protocol-primitives`. Memory:
  `feedback-generic-engine-not-per-manufacturer`.

### Information precedence on conflicts

When sources disagree about a protocol fact:

1. **Live wire captures > everything.** A canonical `camera-initiator`
   observation or btsnoop log is ground truth.
   App-side labels (reference app Java symbol names, Apple `NSString` constants)
   come from human-written source, are more trustworthy than the names
   embedded in camera firmware RAM dumps. Memory:
   `feedback-label-source-precedence`.
3. **Don't infer protocol gating from a constant's name.** Trace which
   layer actually enforces the allowlist/check, then assert. Memory:
   `feedback-verify-protocol-gating-layer`.

## Bootstrap on a clean start

1. **Create a worktree before touching anything** —
   `git worktree add ~/git/ptpsim-<slug> -b <branch>` (SDLC above). The
   primary checkout is read-only for agents; the pre-commit hook rejects
   commits made there.
2. **Evaluate the host you're on — don't assume it from a doc.**
   Sessions run on different Linux and macOS machines. Check the platform your
   harness reports before relying on
   host-specific paths, tools, or capabilities; static host facts
   written into docs go stale and have misled agents before.
3. **Read** your agent's project memory if the environment provides one. Do not
   assume that another contributor has the same agent, memory location, or
   checkout path; durable project instructions belong in this repository.
4. **Check** `gh issue list` for open work. Pending operational tasks
   (CI, correctness, follow-ups) are filed there; this is the project
   backlog.
5. **Skim** the last 10 entries of `git log --oneline` — recent commits
   are the highest-fidelity record of what just changed and why.
6. **Don't** trust any old `RESUME.md`, `resume.sh`, or similar
   session-state file. The one that existed has been deleted; if a new
   one appears, verify it against the repo before relying on it.

## What's outside ptpsim's scope

- iOS app UI / lifecycle — that's the consumer (client application). ptpsim's
  obligation ends at the FFI surface.

  its owning system and surfaced here only as public-safe manifest data,
  reduced evidence, and cited public docs.

  shutdown, and control endpoints; the lifecycle policy lives in the
  consumer.

## Build + test

Run the full CI linux check sequence in this order. The wrapper test runs
first. `fmt --check` then runs before clippy and tests, so a green `cargo test`
alone is not enough to keep `main` building:

```sh
scripts/test-rustc-wrapper.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

`.cargo/config.toml` sets Cargo's workspace-scoped warning policy to `deny`, so
the clippy and test commands enforce warning-free local crates without denying
warnings emitted by dependencies.

CI: Woodpecker, `.woodpecker/`. Steps are path-filtered and heavily cached
(#41), so docs-only pushes run almost nothing. Toolchain images come prebuilt
from the local registry (`ci/images/`). The deployment owner's CI rebuilds
them by manual trigger; `ci-rust` carries the locked workspace dependency
cache, so budget one cold run after a `Cargo.lock` change until it is rebuilt.
The macOS agent keeps a persistent cargo target dir. A commit message containing
`[ALL]` bypasses every path filter when you need a full run.

When a pipeline is red, diagnose it with [`docs/CI.md`](docs/CI.md) before
touching code: an `error` status with no steps is a config-fetch/infra
failure, not a failing test, and rerunning it reuses the stored (failed)
config — create a fresh pipeline instead.
