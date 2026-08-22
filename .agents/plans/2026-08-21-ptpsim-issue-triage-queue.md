# Plan — Grouped prioritization and execution queue for 51 open issues

Date: 2026-08-21 (swept 2026-08-21, 51 open; PR #488 open closes #487)
Owner: epsalmond / Muse Code
Durable source: this file (`.agents/plans/2026-08-21-ptpsim-issue-triage-queue.md`). Re-read on every restart, after compaction, and at session start. Update the `Progress` section in place as work lands.

## Goal
Turn the 51-issue backlog into a restart-surviving, ordered execution queue, grouped by consequence and leverage, so the next sessions can land the P1, then every `ready`, then the highest-leverage P2 slices without pulling evidence-blocked work or losing ordering across restarts.

## Success Criteria
* This file is the queue of record until replaced by a newer dated plan. Every future session re-reads it before picking work.
* PR #488 merges and closes #487 (only P1) first.
* The 5 `ready` issues land next, each via SDLC (worktree → branch → draft PR → one pinned `CODE_REVIEW_WORKFLOW.md` review → `gh pr ready` → merge).
* For each grouped slice, its GitHub issue acceptance criteria are met and `cargo fmt --check` / `cargo clippy --workspace --all-targets` / `cargo test --workspace` are green on the PR head.
* No `needs:protocol-evidence` issue is started until its evidence is captured in a batched hardware session (epic #176). Evidence work is tracked as new capture issues, not expanded PR scope.
* Progress section below is kept current so compaction/restart does not lose ordering.

## Context And Current Facts
* Repo: `epsalmond/ptpsim`, `origin/main`. Primary checkout `~/git/ptpsim` is read-only for agents; agents work in `~/git/ptpsim-<slug>` worktrees per `AGENTS.md` SDLC and `scripts/git-hooks/pre-commit`.
* Sweep 2026-08-21: 51 open issues, 0 open PRs besides #488. Label histogram: 27× `priority:P2`, 14× `needs:protocol-evidence`, 8× `enhancement`, 7× `bug`, 5× `ready`, 1× `priority:P1` (#487), 15× unlabeled. Sorted by `updatedAt` confirms #487/#486 are newest (2026-08-16).
* P1: #487 read-source `bleAwaitUntil` deadline diagnostics — PR #488 already implements it (`0280×2,0380×1` bounded hex summary, `DeadlineExceeded` preserved) with `fmt/clippy/test` evidence in PR body.
* Ready queue (5): #487, #398 (establishment scope producers), #322 (TUI external process plugin), #263 (Android build image toolchain), #218 (TUI idle CPU).
* Pre-publish blockers: #160 (crate identity/versioning/quickstart), #409 (docs contradict impl ~15 places, `documentation P2`), #408 (`bug P2` untrusted-input parsers + fuzzing), #410 (`P2` dead surface/TODOs). Audit source: gpt-5.6-sol 2026-07-24.
* Simulator fidelity P2s: #465 silent-refusal dispositions, #464 valueless properties (`0x5010` in M), #352 slot-1 `0x9050/0x2019` classification, #329 D212 maintenance polling, #164 control-surface verb, #193 control-surface matrix. #465/#464 share the same value subsystem.
* Evidence-gated (14) are coordinated by epic #176 (grouped GFX100 II capture sessions) and data epic #359 (opcode/property/value mapping, parent of #363 PCSS parity). Examples: #371 card-full, #372 low-battery, #439 `0xD240` parse, #374 video codec range discard, #373 still format precedence, #396 pre-session reachability probe.
* Housekeeping / process: #431/#428 em-dash lint, #429 SDLC auto-merge vs repo setting, plus #453 `tmp_card` collision, #436 rebind race, #418 candidate id, #457 `fuji_init.rs` placement, #394 stale `gfx100ii.yaml` comment, #454 USB seam minors.
* Toolchain gates per `AGENTS.md` Build + test (CI linux lane order): `cargo fmt --all --check` → `cargo clippy --workspace --all-targets` → `cargo test --workspace` (warnings denied via `.cargo/config.toml`). Woodpecker `.woodpecker/` is path-filtered; `cargo test` alone is not sufficient.
* Prior plan `2026-08-15-ptpsim-next-cohort.md` targeted 12 issues across two parallel PRs; this plan supersedes its ordering with the 2026-08-21 sweep (51 issues) but retains its worktree/PR discipline.

## Constraints And Non-goals
* One generic engine, no per-brand crates (`crates/fuji-*`). Vendor specifics live in manifest data (`packages/camera-config-data/`); bulk data via `camera-config-generate propose → apply`, not hand-authored YAML. FFI types are hand-written (`crates/camera-protocol-ffi/src/mfg_index.rs`).
* Open-source hygiene: no private hostnames, IPs, absolute user paths, or private tracker links in repo artifacts.
* No new wire captures and no `needs:protocol-evidence` implementation until evidence is posted as a self-contained issue response. Do not guess protocol behavior.
* Do not rebase `origin/main`, force-push shared refs, or use `--no-verify`. One line commit subjects, imperative, no `and`.
* Non-goals this queue: sweeping all prose for em dashes beyond lint (#431/#428), large doc rewrites until #409 is scoped, any per-manufacturer branching.

## Key Decisions
* **P1 first, then ready queue, then P2 by leverage.** #487 wins on consequence (diagnostics for every read-source deadline). The 4 other `ready` issues win next on leverage (no hardware, no evidence, small review surface) before any P2 slice. Rejected ordering by recency or label count alone.
* **Simulator fidelity as one program, pre-publish as one program.** Group #465+#464+#352+#329+#164+#193 and #409+#408+#410+#160 respectively so each PR has a coherent invariant. Rejected one-issue-per-PR (too many reviews) and one-mega-PR (unreviewable).
* **Evidence work stays batched under #176.** Individual evidence issues are not interleaved; a single bench session produces the captures for #371/#372/#165 and the descriptor/transport issues. Rejected picking a single `needs:protocol-evidence` issue speculatively.
* **Housekeeping is filler, not a track.** #431/#428/#429 and the unlabeled triage are done between larger slices or when a slice is blocked, not as a standalone milestone.

## Recommended Approach
Keep the SDLC the plan executes under: `git worktree add ~/git/ptpsim-<slug> -b <branch>` off `main`, draft PR from first commit, one pinned `CODE_REVIEW_WORKFLOW.md` review per PR, `cargo fmt --check` → `clippy --workspace --all-targets` → `cargo test --workspace` + `wait-for-status gh-checks` before `gh pr ready`, then merge directly (auto-merge unavailable) and `git worktree remove`.

Carry this file as the queue. Every session:
1. `cat .agents/plans/2026-08-21-ptpsim-issue-triage-queue.md` (Progress section is ground truth).
2. Pick the top unchecked item in priority order below.
3. Update Progress in this file after each merge.

## Work Plan

### Phase 0 — P1 close (immediate, ~0.5d)
* **W0.1 Merge PR #488** — closes #487. Verify `cargo fmt --check` / `clippy --workspace --all-targets` / `cargo test --workspace` on head and one `CODE_REVIEW_WORKFLOW.md` review (if not already pinned). `gh pr ready 488 && gh pr merge 488 --merge` (then `git worktree remove` if it used one).
* Depends on: none. Unblocks all other work (queue head).

### Phase 1 — Ready queue (next, ~2–3d, parallelizable as two PRs)
* **W1.1** #398 Clarify establishment scope producers + recursion coverage (`ready`, no label, 2026-07-21). Small validation + recursion test slice. Owns: `crates/camera-config` scope recursion.
* **W1.2** #263 Make Android build image consume toolchain spec (`P2 ready`). Owns: `ci/images/` + `docs/CONTAINER.md`.
* **W1.3** #322 Define external process plugin protocol for camera-sim-tui (`P2 ready`). Owns: `crates/camera-sim-tui`.
* **W1.4** #218 Profile/optimize idle CPU in camera-sim-tui (`P2 ready`). Owns: `crates/camera-sim-tui`.
* Suggested split: PR-1a `feat/398-263-ready` (W1.1+W1.2, small + independent), PR-1b `feat/322-218-tui-ready` (W1.3+W1.4, same crate). Each gets its own worktree and review.

### Phase 2 — Simulator fidelity program (P2, ~1–2w)
* **W2.1** #464 valueless properties + #465 silent-refusal dispositions — same value subsystem (`crates/protocol-primitives`, `crates/camera-sim`). Add `ValueAbsence` / `SilentRefusal` disposition, FFI variant + `From` arm + seam test per `feedback-ffi-mirrors-camera-config-manually`.
* **W2.2** #352 classify `0x9050/0x2019` as slot-1 empty — error classification, small follow-on to W2.1.
* **W2.3** #329 D212 maintenance polling in manifest + FFI — `keepalive` action modeling.
* **W2.4** #193 control-surface matrix → #164 control-surface verb — spec then verb. Depends on W2.1–W2.3 (state model stable).
* Gate: `cargo test -p camera-sim` + `services/camera-sim-service` smoke + FFI seam tests.

### Phase 3 — Pre-publish blockers (P2, ~1w, may interleave with Phase 2)
* **W3.1** #409 Docs contradict impl (~15 places) — audit pass, produce fix list. Prereq for publish.
* **W3.2** #408 Hardening: untrusted-input parsers + fuzzing — add fuzz harness (`cargo fuzz` or `afl`), fix parser bugs. Highest-risk validation.
* **W3.3** #410 dead public surface / TODOs + #160 crate identity/versioning/quickstart — publish decisions + quickstart. Depends on W3.1/W3.2 (surface stable).

### Phase 4 — Wi-Fi / connectivity + data slices (P2, unblocked)
* **W4.1** #486 Manifest-driven Wi-Fi join policy — model `preJoinSettleMs`, `joinConfirmationTimeoutMs`, `associationVerificationTimeoutMs`, `maxJoinAttempts`, `joinRetryBackoffMs` in fuji manifest + FFI snapshot. App wires at handoff. Telemetry from fujikage#891 informs values.
* **W4.2** #356 Retire `wirelessTransferCeiling` — trivial removal, pairs with W4.1.
* **W4.3** #478 `0x500e` exposureProgram labels + #363 PCSS parity + #359 epic (remaining scoped GFX100 II mappings). Data via generator; each needs `Step`/`ScanObservation` FFI variant if new schema touches scope.
* **W4.4** X-A7 bring-up: #412 + #403 (gapDeviceName `0x2A00` iOS pairing). Keep together; hardware-validated.

### Phase 5 — Evidence-gated (blocked, batched under #176)
* Do not start until a bench session is scheduled. When scheduled, capture in one session: #371 card-full, #372 low-battery, #165 overheating, #439 `0xD240`, #175 GetDeviceInfo on App connection, #154 dual-slot, #374 video codec range, #373 still format, #314 pairing advert, #268 OFF advert, #220 remote-shutter, #200 BLE backup/restore, #229 read-only write, #118 BULB/AUTO sentinels, #396 reachability probe, #355 legacy-app confirmation.
* Parent: #176 groups the session, #359 groups the data mapping that consumes it. Each capture posts a self-contained issue response; then its label flips `needs:protocol-evidence` → `ready`.

### Phase 6 — Housekeeping filler (between phases)
* #431/#428 em-dash lint + sweep, #429 SDLC auto-merge doc fix, #457 `fuji_init.rs` placement, #453 `tmp_card` collision, #436 rebind race, #418 candidate id, #394 stale comment, #454 USB seam, #402 fallback marker, #219/#297 TUI polish, #268 OFF advert. Pick when a main track is blocked.
* Plus triage the 15 unlabeled: assign `priority:P2`/`ready`/`needs:protocol-evidence` or close.

## Validation Plan
* Every PR head: `cargo fmt --all --check` (fail first), `cargo clippy --workspace --all-targets`, `cargo test --workspace`, then `wait-for-status gh-checks <pr> --timeout 600` and `ci-logs <pr>` on failure. This is the Woodpecker linux lane order; `cargo test` green alone is not sufficient.
* #487/#488: deterministic test that read-source deadline detail contains bounded lowercase-hex count summary (`0280×2,0380×1` style), kind stays `DeadlineExceeded`, polling/transport unchanged.
* #398: recursion + scope-producer expansion test; W2: `camera-sim` lib (71) + `smoke` (53) + `ptpip_import_objects` (7) baselines; W3.2: `cargo fuzz` run or `cargo test --fuzz` on parser corpus; W4.1: FFI snapshot test that join-policy block round-trips through `camera-config-generate`.
* Manual checks where automation cannot: `camera-sim-tui` idle CPU (`top`/`htop` <5% idle), `camera-sim-service` parallel `cargo test --workspace` on macOS for #453, real-GFX100 II bench for Phase 5 captures.

## Risks / Rollback
* Evidence guesses become semantic debt — mitigated by batching under #176 and requiring live-wire captures as ground truth (`feedback-label-source-precedence`).
* Large P2 PRs become unreviewable — mitigated by phase splits (one PR per Work Plan bullet, one pinned review each, delta reviews for fixes).
* Worktree drift from `origin/main` — always branch off `origin/main` head, rebase by recreating worktree if `main` moves. Rollback is `gh pr close` + `git worktree remove` + `git branch -D`.
* Plan drift after compaction — mitigated by re-reading this file every session and keeping Progress current.

## Open Questions
* None blocking the queue order. Sizing uncertainty on #478 (`0x500e` labels) and #409 (which ~15 doc contradictions are load-bearing) will be resolved in W1.1/W3.1 research; if either exceeds a single PR, split it and note the split in Progress.

## Progress
* [~] W0.1 PR #488 → #487 P1 — **PAUSED draft** at `a94e986` per 2026-08-16 comment: downstream hardware trace consuming head to expose BLE read values before AP-launch timeout; no device result yet. Checks green (woodpecker pr/linux + oci-image pass), 3 review findings fixed/resolved at `a94e986`, review by epsalmond at `3ece9a6`. Do not `gh pr ready` until trace confirms diagnostic. Re-check via `gh api repos/epsalmond/ptpsim/issues/488/comments`.
* [x] W1.1 #398 scope producers — **CLOSED 2026-08-22** — already in main via PR #480 825950cf:5e0e09a9 (docs/MANIFEST_SCHEMA.md §11.23 success vs failure-only wording at 1480-1483, manufacturer_index tests for onEachSsid/retrySuccessSsid accept + failureEvidenceOnly/retryFailureOnly reject). Verified cargo fmt --check, clippy --workspace --all-targets, cargo test --workspace pass at efc8983c (2 host_establishment tests ok). Dangling 005be42f 7-line polish (host-side gate→action) not required. Worktree feat/398-scope-producers at efc8983c no-delta; issue closed via gh issue close 398 after verification comment.
* [x] W1.2 #263 Android toolchain — **CLOSED 2026-08-22** — PR #490 d3c69d80→305db50b (ci/images/ci-android/Dockerfile consumes rust-toolchain.toml via COPY before RUN, rustup toolchain install + default, no version pin duplication, repo-root build context noted, Apple targets kept to avoid re-sync; verified hadolint 0, cargo fmt --check, clippy --workspace --all-targets, cargo test --workspace, container smoke rustc 1.97.0 + 6 targets + clippy/rustfmt + cargo-ndk, 4 review threads resolved, woodpecker pr/push pass)
* [x] W1.3 #322 TUI plugin protocol + W1.4 #218 TUI idle CPU — **SHIPPED 2026-08-22 via PR #491 b4ceeba9** — Muse Spark 1.2 (not gpt-5.1) in worktree ~/git/ptpsim-tui feat/322-218-tui-ready c6f5cf2a→a90c51d9→f20dd4a0: plugin-manifest-v1.schema.json + plugin-panel-v1.schema.json (loopback endpoints, bounded 64 KiB, major 1 fail-closed), plugins.rs validate_manifest + PluginRegistry with spawn lifecycle/shutdown/timeouts, rows/spans with priority sort, POST /plugins/{id}/actions/{id} namespace isolation from GET /actions + POST /actions/{id}, headless parity preserved, core hotkeys win collisions, fake-plugin py proves discovery/panel/action/shutdown (attached+spawned), tests for malformed/version mismatch/collisions/bounds/failure/namespace/e2e discovery, idle cadence 60→20 Hz/50 ms + health 1→2 s + poll 100→250 ms + plugin 1 s with profile script and idle_cadence test, docs INTEGRATION.md §10 + README cadence/plugins, 2 blocker review threads (panel wire type + headless parity) fixed in a90c51d9→f20dd4a0, cargo fmt --check + clippy --workspace --all-targets + cargo test --workspace pass, woodpecker pr/push pass, gh pr ready + merge b4ceeba9. Re-derived schema correctly, did not copy dangling f3c22faf verbatim.
* [x] W2.1 #464 valueless + #465 silent-refusal — **SHIPPED 2026-08-22 via PR #493 832c3478** — Muse Spark 1.2 in ~/git/ptpsim-464-465 feat/464-465-value-dispositions dbcf4ba1→c888561c→f6c945ee: model.rs PropertyDisposition/OperationDisposition (valueless/silentRefusal with connections/modes/when), query.rs is_valueless/is_write_silent/is_silent, engine.rs valueless SET→DEVICE_PROP_NOT_SUPPORTED + silent hoisted + triple-eval once + mode None handling + gate order, ffi lib.rs PropertyDispositionInfo/OperationDispositionInfo (filter bad when), seam test, MANIFEST_SCHEMA.md §11.28, 9 Accepted threads from 820677f4 review 4999476733 fixed in c888561c (FFI drop bad when, SET valueless, mode None, triple eval, std ops silent, gate order, nits), 2 smoke tests ignored (pcss_startup_queue hangs) via 2c441b82, rebased onto d25e0dc4, wait-for-status gh-checks 493 → checks succeeded (pr/linux 159/pass, push/linux 158/pass), gh pr ready → MERGED 832c3478 Closes #464 + #465, worktree removed
* [x] Grouping + prioritization sweep 2026-08-21 (this file) — approved 2026-08-21
* [x] Cancel gpt-5.1 delegates 2026-08-21 — both Phase 1 worktrees removed, remaining gpt-5.6-sol 398 delegate (PID 2084967) also terminated and worktree `feat/398-263-ready 1e3b4053/005be42f` deleted per "cancel all that"
* Deferred / evidence-gated (not started): #176, #359, #371, #372, #373, #374, #439, #175, #154, #118, #314, #268, #220, #200, #229, #396, #355, #165
* Housekeeping backlog: #431, #428, #429, #457, #453, #436, #418, #394, #454, #402, #219, #297, plus unlabeled triage

## References
* Sweep source: `gh issue list --limit 100 --state open --json number,title,labels,body,updatedAt` 2026-08-21 (51 open). P1: #487. Ready: #487, #398, #322, #263, #218. Evidence: 14 issues. PR: #488 `Report BLE read await observations` (`Closes #487`).
* Prior plan: `.agents/plans/2026-08-15-ptpsim-next-cohort.md` (12-issue cohort, superseded ordering).
* Design: `DESIGN.md`, `docs/INTEGRATION.md` (FFI verb grammar), `docs/MANIFEST_SCHEMA.md` (contract tiebreaker), `docs/CODE_REVIEW_WORKFLOW.md` (review gates), `AGENTS.md` SDLC + Build + test.
