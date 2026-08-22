# camera-sim-tui

`camera-sim-tui` is the generic ptpsim operator console. It attaches to a
running `camera-sim-service`, subscribes for pushed state snapshots, and renders
the current simulator state with keyboard actions over the same HTTP controls
that scripts can call.

```sh
cargo run -p camera-sim-tui -- \
  --control 127.0.0.1:8080 \
  --listen 127.0.0.1:8770
```

From the workspace root, `scripts/run-tui` builds the TUI, attaches to an
existing local service, or starts `./run` with the default fixture camera first.
Override the default listener with `PTPSIM_TUI_LISTEN`; service defaults are the
same `PTPSIM_*` knobs that `./run` already accepts.

The TUI registers `POST /callbacks {"url":"http://127.0.0.1:8770/state"}` with
the simulator. The service sends the current snapshot immediately and then
pushes later changes. The TUI fetches the manifest action catalog from the
simulator and also serves:

- `GET /actions` — the fetched manifest action catalog, byte-for-byte in the
  same JSON shape as the simulator service.
- `POST /actions/<id>` — proxy a responder-role manifest action after exact
  revision/mode/parameter validation.
- `GET /operator/actions` and `POST /operator/actions/<id>` — inspect or invoke
  process-local phase and quit controls; these are not camera actions.
- `GET /state` — last state snapshot received by the TUI.

The default visual style is `--theme cyberpunk --glyphs unicode`: black
operator-console background, client application fixture TUI-inspired ice-cyan text,
green/yellow/red state color, reverse-video hotkeys, broken Unicode section
lines, and compact operator markers. Use `--glyphs ascii` for terminals that
do not render box drawing or symbols cleanly; `--theme neon` and `--theme mono`
remain available.

The dashboard shows process memory, transferred bytes, transfer rate,
live-view FPS, standard and camera-initiated queue depth/completions, update
rate, uptime, idle time, Rust/toolchain versions, and manifest-backed property
names as `label (0xcode)`.

The draw loop caps visible updates at 20 Hz, crossterm polls at 250 ms idle,
and health/plugin refresh at 2 s / 1 s; pushed state and hotkeys still trigger
immediate redraw. Idle CPU should stay below 5 percent; profile with
`top -pid $(pgrep camera-sim-tui)` or `ps -o %cpu -p <pid>` on the same
host/terminal (see `scripts/profile-tui-idle.sh` recipe).

`--headless` does not render the visual theme; it serves the action/state HTTP
surface and prints the selected `theme`/`glyphs` in its startup JSON so smoke
checks can catch stale binaries or wrong flags.

For CI or agent-driven sessions, run the same attach/action surface without
curses:

```sh
cargo run -p camera-sim-tui -- --headless
```

Plugins are external processes or attached loopback HTTP services.

```sh
cargo run -p camera-sim-tui -- \
  --plugin-manifest tools/camera-sim-tui/fake-plugin/manifest-attached.json \
  --plugin-url http://127.0.0.1:8765
```

See `docs/INTEGRATION.md` §10 and `tools/camera-sim-tui/fake-plugin/` for the
versioned discovery, rows/spans panel payload, plugin/operator namespace
(`POST /plugins/{id}/actions/{id}` distinct from `POST /actions/{id}`),
hotkey collision (core wins), bounded payloads, lifecycle/shutdown, and
headless parity.
