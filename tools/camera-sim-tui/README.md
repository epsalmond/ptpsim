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
pushes later changes. The TUI also serves:

- `GET /actions` — self-describing action registry with hotkeys and HTTP paths.
- `POST /actions/<id>` — invoke the same generic simulator mutation as the
  matching hotkey.
- `GET /state` — last state snapshot received by the TUI.

The default visual style is `--theme cyberpunk --glyphs unicode`: true black
background, muted teal/gray structure, acid green and amber accents, broken
Unicode section lines, and compact operator-console markers. Use
`--glyphs ascii` for terminals that do not render box drawing or symbols
cleanly; `--theme neon` and `--theme mono` remain available.

The dashboard shows process memory, transferred bytes, transfer rate,
live-view FPS, update rate, uptime, idle time, Rust/toolchain versions, and
manifest-backed property names as `label (0xcode)`. The draw loop caps visible
updates at 60 Hz; idle CPU profiling/optimization is tracked separately in
ptpsim #218.

`--headless` does not render the visual theme; it serves the action/state HTTP
surface and prints the selected `theme`/`glyphs` in its startup JSON so smoke
checks can catch stale binaries or wrong flags.

For CI or agent-driven sessions, run the same attach/action surface without
curses:

```sh
cargo run -p camera-sim-tui -- --headless
```
