#!/usr/bin/env bash


#
# Usage:
#   ./resume.sh              # print prompt to stdout

#
# Update RESUME.md if the underlying state changes — that's the actual
# briefing; this script just produces a pointer to it.

cat <<'PROMPT'
Resume the BLE-MVP CI workstream on the ptpsim repo.

START HERE — read these IN ORDER, do not skip:


   The live snapshot: what merged on PR #2 (BLE-MVP P0+P1), what's on the
   ble-mvp branch waiting to PR (9 commits — the xcframework recipe + the
   macOS Woodpecker agent + the Ansible provisioning), what was happening
   at the moment of compaction.


   The verified xcframework build recipe. The spike that verified it ran

   already in the text.



   The CI side. The build script is fully spike-verified. The Ansible



   Cross-session memory; project-ble-mvp-implementation.md has the broader
   project arc.

REPO STATE on resume:

* Branch `main`: BLE-MVP P0+P1 merged (PR #2). 151 tests passing, fmt +
  clippy `-D warnings` clean.
* Branch `ble-mvp`: 9 commits ahead of origin/main with the CI work +
  user's parallel python-wheel commit (4a56e93 — not part of the
  xcframework chain). Pushed; NOT PR'd yet.

  file appeared empty on first verification check just before compaction
  — possibly just race / launchd silence; possibly broken. THIS IS THE
  FIRST THING TO INVESTIGATE.

DO NOT do these (already done — verify if you're unsure):

* Re-run cargo tests; baseline is stable at 151.
* Re-do P0 schema, P1 FFI, or the transform: addition.
* Re-run the §11.11 xcframework spike; it's recorded in §11.11 already.
* Re-write or "improve" the Ansible playbook unless something's
  actually broken on the next run.

NEXT STEPS in order:

A. Verify the Woodpecker macOS agent actually registered with the server.
   Empty log output before compaction is suspicious. Check:



   If broken, fix the Ansible playbook + re-run. If just silent, confirm

   platform=darwin,arch=x86_64,xcode=true.

B. After agent is confirmed live, remind the user to add the
   `github_token` secret in Woodpecker repo settings (fine-grained PAT
   on epsalmond/ptpsim with contents:write). The xcframework step calls
   gh release create/upload which needs it.

C. Open the PR for the ble-mvp follow-up commits — title and body draft
   are in the conversation history (or just synthesize from RESUME.md).
   Hold for the user to merge.

D. After merge: monitor the first xcframework build. A successful run
   produces a GitHub release `sha-<8>` on the merge commit with the
   tarball attached.

E. Tell the iOS team the artifact is available.

If anything in RESUME.md is stale relative to what you observe in the
repo, trust the repo and update RESUME.md.
PROMPT

