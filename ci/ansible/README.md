# `ci/ansible/` — Ansible playbooks for build-side infra

First Ansible work in the repo; will grow during the Docker → VM migration.

## What's here

| file | purpose |
|---|---|
| `ansible.cfg` | inventory pointer + sane defaults |
| `inventory.yml` | hosts, grouped by role (`ci_macos` today) |
| `requirements.yml` | `community.general` for `launchd` + `homebrew` |
| `woodpecker-agent.yml` | provisions a macOS host as a Woodpecker CI agent for explicit Apple FFI promotions |
| `templates/com.woodpecker.agent.plist.j2` | LaunchDaemon plist |

## Provisioning the macOS Woodpecker agent (one-time per host)

Prereqs on your control machine: `ansible-core ≥ 2.16`, SSH access to the
target as a user with passwordless sudo (the inventory defaults to `eric`).

```bash
cd ci/ansible
ansible-galaxy collection install -r requirements.yml

ansible-playbook woodpecker-agent.yml \

    -e woodpecker_agent_secret=<token-from-Woodpecker-admin-UI>
```

Idempotent: re-running upgrades the agent binary, re-renders the plist,
restarts via the handler only if anything changed.

The two `-e` vars don't belong in source. For routine runs, switch to
`ansible-vault` once we have a vault-key story.

## Verify

```bash

```

Look for the agent's "registered with server" message; then confirm it
shows up in the Woodpecker UI (Admin → Agents) with labels
`platform=darwin,arch=…,xcode=true`. Pipeline steps with matching
`labels:` route to it.

## Where the rest of the BLE-MVP CI lives

- `ci/build-xcframework.sh` — the subcommand-based recipe the agent runs.
- `.woodpecker/xcframework.yml` — explicit Apple artifact promotion workflow.
- `docs/APPLE_FFI_RELEASES.md` — supported slices and consumer policy.
- `docs/APPLE_FFI_RELEASES.md` "Build recipe" — design rationale.

The Woodpecker server side supplies the `github_token` repository secret used
to publish release artifacts.
