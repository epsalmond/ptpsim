---
description: The durable PR review-thread workflow — pinning reviews to SHAs, triaging threads, delegating fixes, delta reviews, and readiness gates ending in agent self-merge.
status: reference
read-when: Running or remediating a /code-review pass, resuming a PR after context loss, or deciding whether a PR is review-complete.
---

# Code review workflow — durable PR threads, bounded fixes

Review state lives on the pull request, not in agent chat or an in-process task
list. Context compaction, session restarts, and model changes must not trigger a
new full review merely because the prior conversation is unavailable.

1. **Pin the review to a commit.** The one full review required by the SDLC is

   against an exact candidate SHA. The reviewer does not edit code. It posts one
   PR review thread per actionable finding, inline on the affected line where
   GitHub permits. Otherwise it posts a PR comment naming the exact `path:line`,
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
   review reports no blocking findings. The agent that owns the PR then merges
   it (auto-merge per SDLC step 5) without waiting for a human; after the merge
   lands, send a short merge notification through your environment's operator
   channel if one is configured.

After context loss, resume from the PR head/base SHAs, unresolved review
threads and their triage replies, linked deferred issues, local `git status` and
recent commits, and CI state. Do not reconstruct the review queue from chat and
do not start a fresh full review solely because the session was compacted.
