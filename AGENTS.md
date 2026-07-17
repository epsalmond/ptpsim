

This file is the project-specific override layer for *all* CLI agents. Read any
user- or tool-level instructions supplied by your environment too; this file
takes precedence on anything ptpsim-specific.

## What ptpsim is

A scriptable, OSS camera-protocol simulator (PTP/IP responder). One generic
engine, no per-manufacturer code; cameras are added as manifest **data**
(`packages/camera-config-data/`) authored from probe evidence through the
in-repo generator intake (see DESIGN.md "Operating model"; the predecessor
standalone mapper toolkit is archived). Dual-licensed MIT OR Apache-2.0.

Full design: [`DESIGN.md`](DESIGN.md). Consumer-facing FFI surface and
verb grammar: [`docs/INTEGRATION.md`](docs/INTEGRATION.md). Manifest
schema authority: §11 of
[`docs/plans/ios-rewrite-p0-p1-ble-mvp.md`](docs/plans/ios-rewrite-p0-p1-ble-mvp.md)
(the "contract tiebreaker" — when the code and other docs disagree,
§11 wins).

## Working conventions

### Open-source hygiene — nothing private lands here

This is a public repo. Repo content — docs, code comments, CI config, fixtures —
must stand alone for an outside contributor with no access to the authors'
machines. Never add:

- Private hostnames, IPs, or registry/CI endpoints (internal registries,
  tailnet names, and dev-machine hostnames).
- Absolute or user-specific paths into private checkouts or trees.
- Cross-links into private issue trackers (`<consumer-repo>#123`) — restate the
  durable fact in place instead of linking to where it came from.
- A private consumer's internals (app source paths, backend/service topology).
  Naming a consumer as a motivating example is fine where load-bearing; its
  implementation details are not.

The test: would the line make sense on a stranger's laptop? If not, it belongs
in the consumer's repo or a private tree. Existing violations are tracked
(#255, #160, #259) — do not add new ones while those burn down.

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
`ready` — implementable without new protocol evidence; unlabeled — backlog.

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
3. **Open a draft PR from the first commit.** The draft is the work's visible
   home while in flight; don't wait for polish to push. Before marking it
   ready: run the workspace checks (*Build + test*), then get a reviewer-agent

   medium — and complete the durable review-thread workflow below.
4. **Single-line commit messages.** Imperative (`Add logging for X`), no
   body. The *why* goes in the PR description — or a durable `docs/*.md` if
   it's load-bearing past the merge. Commits should not have "and."
5. **Push the branch, mark the PR ready, a human merges it.** Don't merge your
   own PR unless the user asks — `main` only advances through merged PRs.
6. **Write the PR for a human:** what was done and why it matters, in prose
   a reviewer follows. No opcode/method dumps or
   change-by-change logs — reviewers can read the code for that. Shorter is easier to
   reason about.
7. **Never** force-push, `git reset --hard` shared refs, amend published
   commits, or use `--no-verify`. A failing hook is signal — fix the cause.

Documentation SHOULD precede the code change, not follow it.

Run the workspace checks (*Build + test*) before pushing.

### Code review — durable PR threads, bounded fixes

Review state lives on the pull request, not in agent chat or an in-process task
list. Context compaction, session restarts, and model changes must not trigger a
new full review merely because the prior conversation is unavailable.

1. **Pin the review to a commit.** Run one full reviewer-agent pass against an
   exact candidate SHA. The reviewer does not edit code. It posts one PR review
   thread per actionable finding, inline on the affected line where GitHub
   permits. Otherwise it posts a PR comment naming the exact `path:line`,
   symbol, and reviewed SHA. Every finding states severity, evidence, the
   expected correction, and a verification test. A finding that exists only in
   chat does not count as review state.
2. **Triage every thread durably.** The planning/running agent replies with one
   of: `Accepted`, `Deferred to #N`, `Rejected` with a reason, or `Duplicate`
   with a thread link. Deferred findings require a GitHub issue before the PR
   thread is resolved. Keep accepted threads open while work is in flight.
3. **Delegate accepted findings narrowly.** Give a fixer agent the review-thread
   URL or id, exact base SHA, affected area, and acceptance test. Use a separate
   worktree and branch for each agent; parallelize only non-overlapping fixes.
   The fixer changes only the accepted finding, runs focused tests, makes an
   intentional commit, and replies to the thread with its commit and evidence.
   It does not run another broad review.
4. **Integrate before resolving.** The planning/running agent lands the fixer
   commit on the PR branch, runs proportionate integration checks, pushes it,
   and then resolves the thread. A thread is never resolved merely because a
   fix exists in an unintegrated worktree.
5. **Review deltas, not the whole PR again.** After fixes, review only the range
   from the previously reviewed SHA to the current head. The delta review checks
   that accepted findings were fixed and that their fixes introduced no
   regression. A newly noticed issue in unchanged code becomes a follow-up issue
   unless it is a genuine merge-blocking correctness or safety defect. Record
   the reviewed SHA range on the PR.
6. **Use explicit readiness gates.** A PR is review-complete only when no
   accepted or blocking thread remains unresolved, every deferred thread links
   to an issue, required checks pass on the current head, and the final delta
   review reports no blocking findings. A human still merges.

After context loss, resume from the PR head/base SHAs, unresolved review
threads and their triage replies, linked deferred issues, local `git status` and
recent commits, and CI state. Do not reconstruct the review queue from chat and
do not start a fresh full review solely because the session was compacted.

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
  `Observation` / etc. enums) + a `From` arm + a seam test, or the
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

Run the full check sequence the CI linux lane gates on, in this order —
`fmt --check` runs first and fails the lane before clippy/test, so a green
`cargo test` alone is not enough to keep `main` building:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

`.cargo/config.toml` sets Cargo's workspace-scoped warning policy to `deny`, so
the clippy and test commands enforce warning-free local crates without denying
warnings emitted by dependencies.

CI: Woodpecker, `.woodpecker/`. Steps are path-filtered and heavily
cached (#41): docs-only pushes run almost nothing; toolchain images come
prebuilt from the local registry (`ci/images/`, rebuilt by a manual
trigger of the ci-images workflow — do that to pick up a new rustc, and
budget one cold cache run after); the macOS agent keeps a persistent
cargo target dir. A commit message containing `[ALL]` bypasses every
path filter when you need a full run.
