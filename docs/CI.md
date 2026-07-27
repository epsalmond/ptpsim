---
description: Reading ptpsim's Woodpecker CI — workflow layout, what failure/error/killed statuses mean, and triage rules of thumb.
status: reference
read-when: A pipeline is red or errored, or you are changing .woodpecker/ workflow definitions.
---

# CI — reading a red pipeline

ptpsim CI runs on [Woodpecker](https://woodpecker-ci.org/). Workflow
definitions live in [`.woodpecker/`](../.woodpecker/):

- `linux.yml` — fmt / clippy / test plus release artifacts (push and PR).
- `oci-image.yml` — `camera-sim-service` image build/publish.
- `xcframework.yml` — Apple FFI promotion, tag-triggered only.

Steps are path-filtered and cached; a commit message containing `[ALL]`
bypasses every path filter (see AGENTS.md "Build + test").

## What the statuses mean

- `failure` — your code ran and a step exited non-zero. Open the pipeline and
  read the failed step's log; treat it like a local test failure.
- `error` — the pipeline **never ran your code**: configuration could not be
  fetched or validated, or the runner infrastructure refused the job. These
  pipelines typically have zero steps and zero logs, so there is nothing to
  scroll through — the detail is the error banner on the pipeline itself.
- `killed` — superseded by a newer push on the same ref (the project cancels
  stale runs). Usually benign.

## Rules of thumb

- A red `main` right after a merge is not automatically the merge's fault.
  Check whether any steps ran before reading code.
- An `error` pipeline with no steps is **not** something you fix by editing
  code — and usually not by editing `.woodpecker/` either. Fetch failures
  happen between the CI server and the forge; report them to a maintainer.
- If you *did* change `.woodpecker/`, validate locally first with
  `woodpecker-cli lint .woodpecker/<file>.yml` — schema rejections surface as
  `error` pipelines, not as test failures.
- Rerunning an `error`ed pipeline reuses the configuration stored with it; if
  the fetch failed, the rerun fails the same way. Maintainers retrigger a
  fresh run instead.

The CI instance is privately operated. Instance-level diagnostics (server
logs, agent fleet, config-fetch layer) are documented with the operator's
infrastructure, not in this repository — maintainers can find them there.
