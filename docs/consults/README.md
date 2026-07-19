# Consults

Paired Q&A between ptpsim and its **public-consumer peers** — the client application
iOS app, the Android consumer, other open clients of the ptpsim manifest/FFI.
**Both the request and the reply land here as siblings**, so it's clear that
a question was asked AND answered.

Consults with non-public evidence providers do NOT live here. Those providers
follow their own issue and publication rules, then return a scoped,
self-contained answer to the public ptpsim issue carrying
`needs:protocol-evidence`. A public issue may include a neutral
supporting-analysis link, but that link is never the evidence contract and is
not copied into tracked manifest metadata or documentation. The applicable
scope, behavior, uncertainty, and implementation consequence are restated in
ptpsim, with public reduced evidence added where review requires it. The
provider does not edit ptpsim code, manifests, fixtures, tests, documentation,
or other tracked artifacts directly; maintainers and contributors integrate
the issue response through the normal ptpsim workflow.

The public issue label is a queue state, not an authorization for hardware
work. A maintainer removes `needs:protocol-evidence` and adds `ready` only when
the returned answer makes implementation possible. A sufficient negative
answer resolves or re-scopes the issue without `ready`. An inconclusive answer
retains the evidence label with the smallest missing observation recorded in
the issue.

## Protocol-evidence issue intake

Before applying `needs:protocol-evidence`, a maintainer posts exactly one
active request comment in this form. Replace every angle-bracket field and do
not remove or reflow the HTML marker lines:

```markdown
<!-- ptpsim-protocol-evidence-intake:v1 -->
### Protocol evidence request

**Evidence question**

<!-- ptpsim-protocol-evidence-question:v1:start -->
<one literal, implementation-decisive question>
<!-- ptpsim-protocol-evidence-question:v1:end -->

**Implementation blocker**

<what cannot be implemented or corrected until this is answered>

**Public scope**

<camera/body, firmware, connection mode, client generation, and other bounds;
use "unknown" where the scope itself is part of the question>

**Acceptance signal**

<the smallest public-safe answer or reduced fixture that resolves the ptpsim
decision>
```

The text between the question delimiters is the durable request identity.
Editing the blocker, scope, or acceptance signal requires re-review before an
answer is consumed. Replacing the literal question supersedes the earlier
request and starts a new evidence decision. The issue remains the complete
public record; the comment does not grant hardware or capture authorization.

## Naming

`YYYY-MM-DD-<issuer>-<request|response|answers>-to-<target>-<topic>.md`

Request and reply share the same date prefix and topic suffix so they sort
together. Example:

```
2026-06-02-client application-request-to-ptpsim-wireless-tether.md
2026-06-02-ptpsim-response-to-client application-wireless-tether.md
```

## Frontmatter

Each doc starts with structured frontmatter so the pairing is intrinsic, not
just inferred from filename adjacency.

**Request:**
```yaml
---
event: CONSULT_REQUEST
issuer: <who is asking>
targets: [<who is being asked>]
ts: <date>
status: OPEN | ANSWERED | WITHDRAWN
answered_by: <reply filename, if ANSWERED>
re: <one-line topic>
---
```

**Reply:**
```yaml
---
event: CONSULT_REPLY
issuer: <who is answering>
targets: [<original issuer>]
ts: <date>
status: ANSWERED
re_consult: <request filename>
landed_in: <PR / commit / branch where any code changes live>
durable_facts_lifted_into: [<file paths where reusable knowledge is encoded>]
---
```

## Hygiene rule

**Lift the durable knowledge into the right artifact** — manifest comments,
schema doc-strings, design docs — so the same question doesn't need to be
re-asked. The consult is the trail; the manifest is the authoritative answer.
`durable_facts_lifted_into:` in the reply frontmatter is the receipt.
