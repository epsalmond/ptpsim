# CI runbook — diagnosing Woodpecker pipelines

ptpsim CI runs on [Woodpecker](https://woodpecker-ci.org/). Workflow
definitions live in [`.woodpecker/`](../.woodpecker/):

- `linux.yml` — fmt / clippy / test plus release artifacts (push and PR).
- `oci-image.yml` — `camera-sim-service` image build/publish (privileged lane).
- `xcframework.yml` — Apple FFI promotion, tag-triggered only.

Steps are path-filtered and cached; a commit message containing `[ALL]`
bypasses every path filter. This file covers *what to do when a pipeline is
not green*. Keep it free of private hostnames and endpoints — the Woodpecker
instance URL and tokens are environment config, not repo content.

## Access: `woodpecker-cli`

The CLI is the access path for reading pipeline state from a terminal. It
authenticates via `~/.config/woodpecker/config.yml` or the standard
`WOODPECKER_SERVER` / `WOODPECKER_TOKEN` environment variables. Commands used
below:

```sh
woodpecker-cli repo ls                          # repos the server knows
woodpecker-cli repo show <owner>/<repo>         # server's repo record
woodpecker-cli repo sync                        # refresh forge metadata
woodpecker-cli pipeline ls <owner>/<repo>       # recent pipelines
woodpecker-cli pipeline show <owner>/<repo> <n> # one pipeline
woodpecker-cli pipeline ps <owner>/<repo> <n>   # workflows/steps + states
woodpecker-cli pipeline log show <owner>/<repo> <n> [step]
woodpecker-cli pipeline create --branch <b> <owner>/<repo>  # fresh run
woodpecker-cli pipeline start <owner>/<repo> <n>            # rerun (see below)
woodpecker-cli lint .woodpecker/<file>.yml      # local config validation
```

## Status taxonomy

- `pending` / `running` — config fetched; waiting on or using an agent.
- `success` / `failure` — steps ran. `failure` means a step exited non-zero;
  `pipeline ps` shows which, `pipeline log show` has its output.
- `error` — the pipeline never executed normally: config fetch, config
  validation, secret/image policy, or infra. **Often zero steps and zero
  logs**, so `pipeline ps` and `pipeline log show` print nothing and still
  exit 0. An empty `ps` is not proof of health — check the error detail.
- `killed` — superseded: the repo sets `cancel_previous_pipeline_events` for
  `push`/`pull_request`, so a newer push on the same ref kills older runs.
  Usually benign.
- `canceled` — stopped by a user.

## Where the error detail actually lives

For `error` pipelines the message is on the pipeline object, not in step
logs. Read it in the web UI banner, or via the API `errors[]` field. The API
takes a **numeric repo id** (the CLI takes `owner/name`); discover it with:

```sh
curl -s -H "Authorization: Bearer $TOKEN" "$SERVER/api/user/repos?per_page=100" \
  | jq '.[] | select(.full_name=="<owner>/<repo>") | .id'
curl -s -H "Authorization: Bearer $TOKEN" "$SERVER/api/repos/<id>/pipelines/<n>" \
  | jq '.errors'
```

## Cookbook

**`error`, empty `ps`, empty logs.** Config-fetch or validation failure. Read
`errors[]` as above before assuming anything about your change.

**`errors[]` lists YAML schema messages** (e.g. "Additional property X is not
allowed"). The workflow file is invalid for the server's schema version.
Reproduce locally with `woodpecker-cli lint`; note lint may only warn where
the server hard-fails.

**`could not load config from forge: ... (status: 403)`.** The server could
not fetch `.woodpecker/` for that commit. If other repos build at the same
time, it may still be transient for this one — but it is **server-side
infra, not your PR**. Escalate to the CI operator rather than patching
workflow files.

**`pipeline definition not found` after a rerun.** Expected, and the key
rerun gotcha: `pipeline start <repo> <n>` mints a **new pipeline number** but
**reuses the config stored with the original pipeline** — it does not
refetch. Rerunning a pipeline whose config fetch failed therefore fails
identically; rerunning a previously-green pipeline proves nothing about
current config health. To test config fetch (or recover `main` after a
transient fetch failure), create a fresh pipeline instead:

```sh
woodpecker-cli pipeline create --branch main <owner>/<repo>
```

**Server's repo record looks stale** (wrong default branch, visibility,
webhooks): `woodpecker-cli repo show <owner>/<repo>` to inspect,
`woodpecker-cli repo sync` to refresh from the forge.

**Nothing runs and agents look idle.** Check `woodpecker-cli pipeline queue`;
verify the workflow `labels:` match a live agent's advertised labels — a
label with no matching agent queues forever, it does not error.

## Rules of thumb

- A red `main` right after a merge is not automatically the merge's fault.
  Check whether steps ran at all before reading code.
- Never "fix" a config-fetch 403 by editing `.woodpecker/` — the files never
  left the repo; the fetch did.
- After any infra-side fix, verify with a fresh `pipeline create`, not a
  rerun of the errored pipeline.
