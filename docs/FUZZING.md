---
description: Local cargo-fuzz parser targets and synthetic seeds
status: reference
read-when: Building or running parser fuzz targets
---

# Parser fuzzing

The `fuzz/` package runs cargo-fuzz against existing byte-input parser APIs.
It has its own workspace because libFuzzer sanitizer builds use nightly Rust.
Normal workspace builds do not compile these targets or libFuzzer.
All seeds are synthetic. Targets use byte slices and in-memory streams.

## Setup

Install nightly Rust, a C++ compiler, and cargo-fuzz:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz --locked
```

Run these commands from the repository root:

```sh
RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo +nightly fuzz build
cargo +nightly fuzz list
RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo +nightly fuzz run http_service -- -max_total_time=60 -max_len=262145 -rss_limit_mb=2048
```

The empty wrapper variables disable local compiler caches during sanitizer
builds. Use `-max_total_time` to bound each run. Use `-max_len=1048589` for
service data or larger framing inputs. Build and run one target by adding its
name after `build` or `run`.

## Target inventory

| Target | Existing API coverage |
| --- | --- |
| `http_service` | Service request parser, full input and selected prefixes |
| `http_tui` | TUI request parser, full input and selected prefixes |
| `service_frames` | Inbound role-limited frame reader over an in-memory slice |
| `service_data` | Standard, compressed, and USB data-in collection |
| `pcss` | Discovery, notify, init, init acknowledgement, identity comparison |
| `ptp_packets` | Standard framing, container bodies, encode/decode round trips |
| `ptp_datasets` | DeviceInfo, StorageInfo, ObjectInfo, DevicePropDesc, values, strings, arrays |
| `vendor_codecs` | Fuji/USB framing, app init, legacy init, live view, record streams |
| `ffi_codecs` | Exported response, data, event, dataset, init, and handle-list parsers |

Parser errors are ordinary results. Successful packet decodes must remain
stable when encoded and decoded again. Async targets use a current-thread
runtime and finite in-memory input. They do not wait for additional bytes.
`service_data` tries the same input with all three framing variants and
transaction id 1. HTTP targets also parse selected incomplete prefixes.

## Seeds

Each target has named seeds under `fuzz/corpus/`. Seeds use public packet
layouts and parser boundaries. They include valid requests and packets,
large declared lengths, duplicate Content-Length, truncated headers,
unaligned parameters, trailing dataset bytes, and complete data phases.
Add small synthetic reproductions as named seeds when useful. The `.gitignore`
keeps libFuzzer's generated hash-named mutations out of tracked content.

```sh
RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo +nightly fuzz run ptp_packets -- -runs=1000 -max_len=4096
RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= cargo +nightly fuzz tmin ptp_packets fuzz/artifacts/ptp_packets/crash-EXAMPLE
```

Crash artifacts, coverage output, and fuzz build products are ignored. Keep
reduced regression tests in the owning crate when changing parser behavior.

## Current scope

This scaffold has no CI job. `cargo fuzz build` compiles every target, and a
bounded local run replays its seeds before mutation. The main workspace CI
continues to run its ordinary checks.

Private executor transaction assembly, camera-initiator buffered reads,
whole-request deadline simulation, Nikon connection configuration, and
RAF/JPEG file preview parsing need additional harness work. They are not
covered by these byte-input targets. Issue #408 tracks that remaining scope.
