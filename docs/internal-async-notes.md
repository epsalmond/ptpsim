# Async audit: camera-sim + camera-sim-service

Audit for issue #27 (2026-06-10): every `.lock().await` and `tokio::spawn`
in `crates/camera-sim` and `services/camera-sim-service`, with the decision
taken per site. Re-audit when a new `.await` lands inside a lock guard's
scope or a new spawn site appears.

`crates/camera-sim` holds **no locks and spawns no tasks** — the engine is a
synchronous state machine (`Engine::on_operation` is plain `&mut self`). All
async surface lives in the service, which wraps the engine in
`Arc<tokio::sync::Mutex<Engine>>`.

## Lock sites (`services/camera-sim-service`)

| site | guard scope | `.await` under guard? | decision |
|---|---|---|---|
| `lib.rs` `stream_liveview` — `engine.lock().await.phase()` | temporary, dropped at end of statement | none | OK as-is. Copies the `Phase` out. |
| `lib.rs` `stream_liveview` — `frames.lock().await.next_frame()` | temporary, dropped at end of `let` | none | OK as-is. The frame is cloned out; the `write_all` runs after the guard drops, so one slow liveview client never serializes other clients against the frame source. Comment added at the site. |
| `lib.rs` `handle_command_conn` — init ack (`engine.lock()` → `manifest().camera.model.clone()`) | block expression, dropped before the socket write | none | OK as-is. |
| `lib.rs` `handle_command_conn` — op dispatch (`engine.lock()` → `on_operation(...)`) | block expression, dropped before `write_reply` | none (`on_operation` is sync) | OK as-is. This is the intended serialization point: one engine = one camera = serialized operations, matching real-camera semantics. `Reply::DataStream` returns the `ByteSource` by value, so multi-GB streaming in `write_reply` runs **outside** the engine lock. |
| `control.rs` `handle` — `engine.lock().await.state().session_open` | temporary | none | OK as-is. |

No `Mutex` is held across an `.await` anywhere in the service. The
`Engine` mutex stays a `tokio::sync::Mutex` (not `RwLock`): every consumer
either mutates or is so short-lived the distinction is noise.

## Spawn sites

| site | task | lifetime decision |
|---|---|---|
| `lib.rs` command accept loop | `handle_command_conn` per connection | **JoinSet.** Reaped via a `join_next()` select arm; aborted when the loop future is dropped (run()'s `select!` on shutdown). Cutting in-flight commands on exit is run()'s documented contract. |
| `lib.rs` control accept loop | `control::handle` per connection | **JoinSet**, same pattern. (The spawn itself landed with #26 so an idle client can't block `/healthz`.) |
| `lib.rs` liveview accept loop | `stream_liveview` per connection | **JoinSet**, same pattern. Additionally the task now watches the read half for EOF — previously a client that disconnected while the engine was not streaming left its task ticking forever, because only a frame write could surface the broken pipe. |
| `main.rs` signal listener | SIGTERM/ctrl-c → shutdown oneshot | **Orphan on purpose.** One task for the life of the process; it resolves the shutdown channel and the process exits. Nothing to bound. |

Under repeated bind/run/shutdown cycles (smoke tests, restart loops) the
JoinSets are what prevent task accumulation: dropping the accept-loop future
drops its JoinSet, which aborts every per-connection task it owns.
`tests/smoke.rs::bind_teardown_loop_with_live_connections_is_clean` exercises
exactly this with held-open command/control/liveview connections per cycle.
