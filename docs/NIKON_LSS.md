---
description: Spec of the sans-I/O Nikon Linkage Setting Service (LSS) authentication and encrypted-field primitive implemented by protocol-primitives, with clean-room provenance.
status: reference
read-when: Working on Nikon BLE authentication or LSS crypto, or verifying the clean-room boundary of protocol-primitives.
---

# Nikon Linkage Setting Service authentication

This document specifies the finite, sans-I/O Nikon Linkage Setting Service
(LSS) authentication and encrypted-field primitive implemented by
`protocol-primitives`. It describes interoperable wire behavior, not a Nikon
library API. BLE discovery, CCCD writes, timeouts, bonding, and Wi-Fi joining
remain host responsibilities.

## Clean-room boundary and provenance

The compatibility facts below were reconstructed in two passes. A specification
pass compared independently authored public implementations and a static,
black-box pass against SnapBridge 2.13.3's x86_64 LSS library. An independent
Rust implementation was then checked only against explicit oracle inputs,
outputs, and status behavior. The result is app/library-scoped and provisional:
no Nikon camera was operated, and this document makes no D850 or real-camera
interoperability claim.

The black-box pass identified package `com.nikon.snapbridge.cmru` from the APK
with SHA-256
`23ab95fcda744fac3877ef5f3a62fb972728cdb4cb394ab42203ea07c03d294f`.
The oracle member was `lib/x86_64/libLsSec-jni.so`, SHA-256
`18422bf9f1a3f4056759458f60400b8235c056956ee91443cce692ffaf5b5010`.
These hashes document provenance; neither artifact is distributed here.

Public authored sources used by the specification pass:

- `gkoh/furble` commit
  `246de0861b8907a68eec3f2496dcfc666f41816b`,
  `lib/furble/Nikon.{h,cpp}`: four-stage record layout, fixed proof key,
  eight authentication-table rows, and client proof ordering.
- `hurui200320/nsg` commit
  `5dbf1bf234718b1ef8bf360dc521e9095d902256`,
  `kotlin-poc/src/main/kotlin/info/skyblond/nsg/protocol/` and matching tests:
  an independent proof implementation and a captured stage-1/2/3 vector.
- `attilaolah/birdcam` commit
  `93ffc86a85a474dd884e4ac0168a991c1fdb1822`, only the authored
  `nikon/ls_sec.{go,cc,h}`, `nikon/ls_sec/entangle.{cc,h}`, and
  `nikon/coolpix/a900_connection.go` sources: encrypted-field alignment, the
  context API surface, and connection-configuration field lengths. Its inferred
  session expansion was not used as a compatibility source; the native oracle
  established that the payload cipher is standard Blowfish.



output, native context blob, raw cipher schedule, or per-session key belongs in
this repository. Black-box native-oracle checks may retain only explicit test
inputs, outputs, and status codes.

## Authentication records

Every record is exactly 17 bytes:

```text
offset  size  meaning
0       1     stage (1, 2, 3, or 4)
1       8     first stage value
9       8     second stage value
```

The two 8-byte values are copied byte-for-byte. The derivation below is expressed
as byte slices, not native-endian integer fields.

The client begins with an 8-byte persistent client device id and a fresh 8-byte
client nonce. The responder begins with a fresh 8-byte server nonce, a persistent
8-byte server device id, and one authentication-table selection from `0..=7`.

1. Client sends `[1][clientNonce][clientDeviceId]`.
2. Camera sends
   `[2][cameraNonce][proof(selection, cameraNonce, clientNonce)]`.
3. Client tries all eight rows, rejects the record if none matches, and sends
   `[3][clientNonce][proof(selection, clientNonce, cameraNonce)]`.
4. Camera verifies both stage-3 values and sends
   `[4][cameraNonce][serverDeviceId]`. The client treats the first stage-4 nonce
   as opaque; the second value is the persistent server identity used for key
   derivation. A responder uses the canonical echo of its server nonce.

Records with any other length, a stage unexpected in the current role/state, or
a mismatched proof fail closed. Failed authentication does not create a cipher
session. A failed proof leaves that role at the proof-verification state so a
corrected record can be retried. A new BLE authentication attempt still requires
a fresh client nonce; retries of the whole establishment step must not silently
reuse runtime entropy.

## Authentication proof

The proof is a three-block CBC-MAC-like construction over standard Blowfish.
The fixed compatibility key is:

```text
ff ff aa 55 11 22 33 00
```

The initial chaining block is `01 02 03 04 05 06 07 08`. For every input block,
XOR it with the previous chaining block and Blowfish-encrypt the result. The
final encrypted block is the 8-byte proof.

The first input block is one row of the following table, with both words encoded
big-endian. The second and third blocks are the two 8-byte record values in
wire order.

| Selection | Word 0 | Word 1 |
| ---: | ---: | ---: |
| 0 | `704066e4` | `0433d552` |
| 1 | `ed4b8fac` | `15f7e47b` |
| 2 | `24471f11` | `8b5ea1fc` |
| 3 | `05960c31` | `2b8c7f41` |
| 4 | `fda588c1` | `eba8b1f3` |
| 5 | `99166056` | `1bd3d550` |
| 6 | `cd32687f` | `a9e28a30` |
| 7 | `2a8fe834` | `dec7ebf4` |

These values are protocol compatibility constants, not device secrets.

## Session cipher

After stage 4, both roles form this 8-byte handshake summary:

```text
Q = selection || cameraNonce[1..4] || clientNonce[0..4]
```

The ranges are half-open: three bytes from `cameraNonce` and four from
`clientNonce`. Equivalently, copy the first four camera-nonce bytes, overwrite
the first byte with the selection, and append the first four client-nonce bytes.

The session-key seed is the same three-block authentication hash construction,
without a table-row prefix, over these blocks in order:

1. `serverDeviceId` in wire order;
2. `clientDeviceId` in wire order;
3. `Q` in wire order.

The resulting 8 bytes initialize standard Blowfish. Each `encrypt` or `decrypt`
call operates on zero or more complete 8-byte blocks using CBC with an all-zero
IV and no padding. The IV resets for every call. This matters for connection
configuration: SSID and password are separate cipher calls, not one concatenated
stream. A nonzero length that is not divisible by 8 is an alignment error.

The derived seed and expanded cipher schedule are opaque and zeroized on drop.
They are excluded from manifests, FFI, logs, traces, and serialization. An
in-process checkpoint retains only compact semantic inputs and regenerates the
schedule on restore; it exposes no bytes and zeroizes the retained nonces and
identities on drop. Native context tokens and native expanded-state blobs are
build-scoped implementation details, never compatibility fixtures.

## Connection configuration

The connection-configuration characteristic is one read with this finite
layout:

```text
flags: u8
if flags bit 0:
    encrypted SSID: 32 bytes (decrypt independently, right-NUL-padded UTF-8)
    encrypted password: 64 bytes (decrypt independently, right-NUL-padded UTF-8)
    security mode: u8 (0=open, 1=WPA2, 2=WPA3, 3=WPA2/WPA3)
if flags bit 1:
    SPP maximum length: little-endian u32
```

Truncated fields, invalid padding/UTF-8, unknown security modes, and trailing
bytes fail rather than producing partial credentials. Decrypted credentials are
ordinary step outputs, but authentication keys and cipher context never enter
manifest scope.

## Validation contract

Tests reproduce the native oracle's complete 17-byte records for all eight
selections and both roles, plus its byte-exact 32-byte and 64-byte payload cipher
vectors. They also cover the independently published proof vector, mutated
stage-2 and stage-3 proofs with successful corrected-record retry, illegal state
transitions, malformed lengths, zero-length cipher input, canonical stage 4,
the client's ignored stage-4 nonce, semantic context restore against the native
ciphertext, connection-configuration parsing, and redacted debug output. Only
explicit inputs, outputs, and status behavior are retained; the oracle corpus
contains no session key, expanded schedule, or native context blob.
