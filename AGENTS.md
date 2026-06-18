

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
   checkout. `git worktree remove` when done.
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


   (or your agent's equivalent memory index). The index files name the
   project arc, conventions, and references.
2. **Check** `gh issue list` for open work. Pending operational tasks
   (CI, correctness, follow-ups) are filed there; this is the project
   backlog.
3. **Skim** the last 10 entries of `git log --oneline` — recent commits
   are the highest-fidelity record of what just changed and why.
4. **Don't** trust any old `RESUME.md`, `resume.sh`, or similar
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

```sh
cargo test --workspace                              # Rust workspace
```

CI: Woodpecker, `.woodpecker/`. Steps are path-filtered and heavily
cached (#41): docs-only pushes run almost nothing; toolchain images come
prebuilt from the local registry (`ci/images/`, rebuilt by a manual
trigger of the ci-images workflow — do that to pick up a new rustc, and
budget one cold cache run after); the macOS agent keeps a persistent
cargo target dir. A commit message containing `[ALL]` bypasses every
path filter when you need a full run.
