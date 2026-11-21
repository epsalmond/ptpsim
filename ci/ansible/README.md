# Legacy macOS agent provisioning

These Ansible files provision a Woodpecker macOS agent for existing operators.
The current Apple FFI workflow uses GitHub-hosted runners and does not depend
on this agent. See [the CI reference](../../docs/CI.md) and
[Apple FFI releases](../../docs/APPLE_FFI_RELEASES.md) for the active workflow.

The playbook and template remain available to operators who maintain an
existing installation. Inventory, server address, and agent credentials must
be supplied by that installation. Do not store credentials in this repository.

Install the `community.general` collection from `requirements.yml` before
running `woodpecker-agent.yml`. The playbook creates the service account,
installs Rust and Apple targets, and configures the agent's LaunchDaemon.
It does not publish release artifacts.
