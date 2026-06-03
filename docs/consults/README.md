# Consults

Paired Q&A between ptpsim and its peers (client application iOS app, D3-wire, etc.).
**Both the request and the reply land here as siblings**, so it's clear that a
question was asked AND answered.

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


in their respective repos; reference the other side via `prior_consult:` or
`re_consult:` rather than duplicating.
