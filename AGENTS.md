

This file is the project-specific override layer for *all* CLI agents. It


takes precedence on anything ptpsim-specific.

## What ptpsim is

A scriptable, OSS camera-protocol simulator (PTP/IP responder). One generic
engine, no per-manufacturer code; cameras are added as manifest **data**
(`packages/camera-config-data/`) authored from external probe evidence such as
`epsalmond/camera-protocol-mapper`. Dual-licensed MIT OR Apache-2.0.

Full design: [`DESIGN.md`](DESIGN.md). Consumer-facing FFI surface and
verb grammar: [`docs/INTEGRATION.md`](docs/INTEGRATION.md). Manifest
schema authority: §11 of
[`docs/plans/ios-rewrite-p0-p1-ble-mvp.md`](docs/plans/ios-rewrite-p0-p1-ble-mvp.md)
(the "contract tiebreaker" — when the code and other docs disagree,
§11 wins).

## Working conventions

### Work tracking — GitHub issues, not in-process task lists

**Work is tracked as GitHub issues.** Agent in-process task lists

model swaps, and the user does swap between models and agents. Anything worth
remembering across sessions goes in `gh` so it's discoverable from
`gh issue list`.

This means: when you surface a follow-up, a deferred fix, a code-review
finding, or a "we should do X later" — file an issue. The in-process
list is for *this turn's* working memory, not the project backlog.

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
3. **Single-line commit messages.** Imperative (`Add logging for X`), no
   body. The *why* goes in the PR description — or a durable `docs/*.md` if
   it's load-bearing past the merge. Commits should not have "and."
4. **Push the branch, open the PR, a human merges it.** Don't merge your
   own PR unless the user asks — `main` only advances through merged PRs.
5. **Write the PR for a human:** what was done and why it matters, in prose
   a reviewer follows. No opcode/method dumps or
   change-by-change logs — reviewers can read the code for that. Shorter is easier to
   reason about.
6. **Never** force-push, `git reset --hard` shared refs, amend published
   commits, or use `--no-verify`. A failing hook is signal — fix the cause.

Documentation SHOULD precede the code change, not follow it.

Run the workspace checks (*Build + test*) before pushing.

### Querying the Fuji operator cohort

When a private protocol, firmware, wire-format, or mobile-RE question is not
answered by this repo, ask the Fuji operator cohort instead of guessing or
copying RE narrative into ptpsim. The tool runs on nas; from any other host,
prefix with `ssh nas`.

- Discover routing with `fuji-ask-operator --list`.
- Ask questions with `fuji-ask-operator <op> "<question>"`.
  ASK mode runs with host access by default so operators can inspect local
  captures, graph data, and sibling repos. The prompt contract still forbids
  permanent changes: temporary scratch files are okay when needed, but repo
  files, graph facts, devices, persistent config, and other durable state stay
  untouched.
- Delegate scoped work with `fuji-ask-operator --do <op> "<task>"`.
- File async questions with `fuji-ask-operator --consult --as <role> <op> "<question>"`.
  Use the cohort role whose work you represent, for example `--as W3` when the
  question is on behalf of client application.

Before using `--do`, confirm scope with the human when the work drives
hardware, takes a new capture, writes persistence, or is otherwise irreversible.
Analyzing already-captured data or static artifacts may proceed without that
extra confirmation.

Routing guide: USB-PTP/PTP-IP/BLE/XLV/propcodes/FW-update -> `wire` (D3);
reference app APK/FF0018API.so/iOS RE -> `mobile` (D6); SCP108A Linux/Flask/rpmsg ->
`linux` (D4); camera parser/dispatcher/ThreadX/cfgdata/FF80 -> `fw-ff80`
(D1); X-Processor 5 hardware/MMU/bootrom/sensors -> `soc` (D5);
ff80rs/codeexec tooling -> `tools` (X1); knowledge-graph hygiene ->
`curator` (X2).

Search first, spawn second: each call is a full operator session. Do not inline
the operator's answer as RE narrative here. Lift the durable conclusion into
the manifest comment, schema doc string, or `DESIGN.md`, and reference the
operator/spec path that supports it.

The existing `docs/consults/` flow remains for public-consumer contract
negotiation with the client application app. `fuji-ask-operator` is for private RE lookups
behind a manifest entry; it complements that process and does not replace it.

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

- **Manifest data** lives in `packages/camera-config-data/`. Authored
  by people, generated from `camera-protocol-mapper` runs, reviewed before
  merge. Agents don't hand-author YAML manifests — they can edit small
  curated entries (gatt catalog additions, establishment-step
  ordering), but bulk data flows through the
  camera-protocol-mapper → generator pipeline. Memory:
  `feedback-data-via-generator-not-agent-authoring`.


  trees. Reference them by path in comments where load-bearing, but
  don't inline the content. Memory: `feedback-no-operations-in-ptpsim-source`.
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

1. **Live wire captures > everything.** A `camera-protocol-mapper` capture or
   btsnoop log is ground truth.
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

   macOS). Check the platform your harness reports before relying on
   host-specific paths, tools, or capabilities; static host facts
   written into docs go stale and have misled agents before.


   the encoded path is the checkout's absolute path with `/` → `-`, so
   it differs per host — e.g. `-home-eric-git-ptpsim` on the Linux
   host). The index files name the project arc, conventions, and
   references.
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

  that tree's own sessions, surfaced to ptpsim as updated manifest
  data + cited docs.

  shutdown, and control endpoints; the lifecycle policy lives in the
  consumer.

## Build + test

Run the full check sequence the CI linux lane gates on, in this order —
`fmt --check` runs first and fails the lane before clippy/test, so a green
`cargo test` alone is not enough to keep `main` building:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI: Woodpecker, `.woodpecker/`. Steps are path-filtered and heavily
cached (#41): docs-only pushes run almost nothing; toolchain images come
prebuilt from the local registry (`ci/images/`, rebuilt by a manual
trigger of the ci-images workflow — do that to pick up a new rustc, and
budget one cold cache run after); the macOS agent keeps a persistent
cargo target dir. A commit message containing `[ALL]` bypasses every
path filter when you need a full run.
