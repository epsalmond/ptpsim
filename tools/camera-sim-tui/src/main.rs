use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use camera_sim_tui::{
    callback_url_for, Action, ActionKind, ActionRegistry, CameraSnapshot, ControlClient,
    HealthSnapshot, QueueSnapshot, TraceSnapshot,
};
use clap::{Parser, ValueEnum};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Gauge, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

const MAX_UPDATE_HZ: f64 = 60.0;
const MAX_FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
const HEALTH_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const RATATUI_VERSION: &str = "0.30.2";
const CROSSTERM_VERSION: &str = "0.29";
const RUSTC_VERSION: &str = env!("PTPSIM_TUI_RUSTC_VERSION");

#[derive(Parser)]
#[command(
    name = "camera-sim-tui",
    about = "Colorful ptpsim operator console over the generic control API"
)]
struct Args {
    /// camera-sim-service control endpoint.
    #[arg(long, default_value = "127.0.0.1:8080")]
    control: String,
    /// Local TUI HTTP listener for pushed /state callbacks and /actions.
    #[arg(long, default_value = "127.0.0.1:8770")]
    listen: String,
    /// Advertised callback URL. Defaults to http://<listen>/state.
    #[arg(long)]
    callback_url: Option<String>,
    /// Run the HTTP aggregation/action surface without drawing curses UI.
    #[arg(long)]
    headless: bool,
    /// Skip runtime callback subscription; useful against older services.
    #[arg(long)]
    no_subscribe: bool,
    /// Visual theme.
    #[arg(long, value_enum, default_value_t = ThemeName::Cyberpunk)]
    theme: ThemeName,
    /// Terminal glyph set.
    #[arg(long, value_enum, default_value_t = GlyphMode::Unicode)]
    glyphs: GlyphMode,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ThemeName {
    Cyberpunk,
    Neon,
    Mono,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum GlyphMode {
    Unicode,
    Ascii,
}

#[derive(Debug, Clone, Copy)]
struct Theme {
    bg: Color,
    panel: Color,
    panel_hi: Color,
    text: Color,
    muted: Color,
    cyan: Color,
    magenta: Color,
    green: Color,
    yellow: Color,
    red: Color,
    blue: Color,
}

impl ThemeName {
    fn as_str(self) -> &'static str {
        match self {
            ThemeName::Cyberpunk => "cyberpunk",
            ThemeName::Neon => "neon",
            ThemeName::Mono => "mono",
        }
    }

    fn theme(self) -> Theme {
        match self {
            ThemeName::Cyberpunk => Theme {
                bg: Color::Black,
                panel: Color::Black,
                panel_hi: Color::Indexed(236),
                text: Color::LightCyan,
                muted: Color::DarkGray,
                cyan: Color::LightCyan,
                magenta: Color::Indexed(179),
                green: Color::Green,
                yellow: Color::Yellow,
                red: Color::LightRed,
                blue: Color::Blue,
            },
            ThemeName::Neon => Theme {
                bg: Color::Black,
                panel: Color::Black,
                panel_hi: Color::Rgb(22, 22, 28),
                text: Color::Rgb(231, 244, 255),
                muted: Color::Rgb(113, 130, 156),
                cyan: Color::Rgb(0, 229, 255),
                magenta: Color::Rgb(255, 61, 190),
                green: Color::Rgb(51, 255, 153),
                yellow: Color::Rgb(255, 212, 71),
                red: Color::Rgb(255, 80, 92),
                blue: Color::Rgb(89, 157, 255),
            },
            ThemeName::Mono => Theme {
                bg: Color::Black,
                panel: Color::Black,
                panel_hi: Color::DarkGray,
                text: Color::White,
                muted: Color::Gray,
                cyan: Color::Cyan,
                magenta: Color::Magenta,
                green: Color::Green,
                yellow: Color::Yellow,
                red: Color::Red,
                blue: Color::Blue,
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Glyphs {
    border: BorderType,
    brand: &'static str,
    separator: &'static str,
    bullet: &'static str,
    online: &'static str,
    offline: &'static str,
    signal: &'static str,
    key_left: &'static str,
    key_right: &'static str,
    kv_sep: &'static str,
    event_info: &'static str,
    event_state: &'static str,
    event_action: &'static str,
    event_error: &'static str,
}

impl GlyphMode {
    fn as_str(self) -> &'static str {
        match self {
            GlyphMode::Unicode => "unicode",
            GlyphMode::Ascii => "ascii",
        }
    }

    fn glyphs(self) -> Glyphs {
        match self {
            GlyphMode::Unicode => Glyphs {
                border: BorderType::LightDoubleDashed,
                brand: "◢",
                separator: "╱",
                bullet: "◆",
                online: "●",
                offline: "○",
                signal: "▸",
                key_left: "⟦",
                key_right: "⟧",
                kv_sep: "│",
                event_info: "◇",
                event_state: "◆",
                event_action: "▶",
                event_error: "▲",
            },
            GlyphMode::Ascii => Glyphs {
                border: BorderType::Plain,
                brand: "#",
                separator: "//",
                bullet: "*",
                online: "ON",
                offline: "--",
                signal: ">",
                key_left: "[",
                key_right: "]",
                kv_sep: ":",
                event_info: "INFO",
                event_state: "PUSH",
                event_action: "ACT",
                event_error: "ERR",
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ConsoleStyle {
    theme_name: ThemeName,
    theme: Theme,
    glyph_mode: GlyphMode,
    glyphs: Glyphs,
}

impl ConsoleStyle {
    fn new(theme_name: ThemeName, glyph_mode: GlyphMode) -> Self {
        Self {
            theme_name,
            theme: theme_name.theme(),
            glyph_mode,
            glyphs: glyph_mode.glyphs(),
        }
    }
}

#[derive(Debug, Clone)]
enum RuntimeEvent {
    State(CameraSnapshot),
    Action(String),
    Error(String),
    Quit,
}

#[derive(Debug, Clone, Copy)]
enum LogKind {
    Info,
    State,
    Action,
    Error,
}

#[derive(Debug, Clone)]
struct EventLine {
    kind: LogKind,
    text: String,
}

#[derive(Debug, Clone, Copy, Default)]
struct Rates {
    transfer_bps: f64,
    liveview_fps: f64,
    update_hz: f64,
}

struct App {
    registry: ActionRegistry,
    client: ControlClient,
    style: ConsoleStyle,
    listen_addr: SocketAddr,
    callback_url: String,
    health: Option<HealthSnapshot>,
    snapshot: CameraSnapshot,
    events: VecDeque<EventLine>,
    quit: bool,
    rates: Rates,
    last_health_sample: Option<HealthSample>,
    last_health_refresh: Instant,
    trace_cursor: u64,
    trace_instance_id: Option<String>,
    frame_window_started: Instant,
    frames_in_window: u32,
}

#[derive(Debug, Clone, Copy)]
struct HealthSample {
    at: Instant,
    bytes_transferred: u64,
    liveview_frames: u64,
}

impl App {
    fn new(
        registry: ActionRegistry,
        client: ControlClient,
        style: ConsoleStyle,
        listen_addr: SocketAddr,
        callback_url: String,
    ) -> Self {
        let now = Instant::now();
        Self {
            registry,
            client,
            style,
            listen_addr,
            callback_url,
            health: None,
            snapshot: CameraSnapshot::default(),
            events: VecDeque::new(),
            quit: false,
            rates: Rates::default(),
            last_health_sample: None,
            last_health_refresh: now,
            trace_cursor: 0,
            trace_instance_id: None,
            frame_window_started: now,
            frames_in_window: 0,
        }
    }

    fn log(&mut self, kind: LogKind, text: impl Into<String>) {
        self.events.push_front(EventLine {
            kind,
            text: text.into(),
        });
        while self.events.len() > 80 {
            self.events.pop_back();
        }
    }

    fn set_state(&mut self, state: CameraSnapshot) {
        let phase = state.phase.clone();
        let objects = state.media.objects;
        let standard_queue = queue_text(state.transfer_queues.standard.as_ref());
        let camera_queue = queue_text(state.transfer_queues.camera_initiated.as_ref());
        self.snapshot = state;
        self.log(
            LogKind::State,
            format!(
                "state push: phase={phase} objects={objects} standard={standard_queue} camera={camera_queue}"
            ),
        );
    }

    fn set_health(&mut self, health: HealthSnapshot) {
        if self
            .health
            .as_ref()
            .is_some_and(|current| current.instance_id != health.instance_id)
        {
            self.trace_cursor = 0;
            self.trace_instance_id = None;
        }
        let now = Instant::now();
        if let Some(prev) = self.last_health_sample {
            let elapsed = now.saturating_duration_since(prev.at).as_secs_f64();
            if elapsed > 0.0 {
                self.rates.transfer_bps = health
                    .metrics
                    .bytes_transferred
                    .saturating_sub(prev.bytes_transferred)
                    as f64
                    / elapsed;
                self.rates.liveview_fps = health
                    .metrics
                    .liveview_frames
                    .saturating_sub(prev.liveview_frames)
                    as f64
                    / elapsed;
            }
        }
        self.last_health_sample = Some(HealthSample {
            at: now,
            bytes_transferred: health.metrics.bytes_transferred,
            liveview_frames: health.metrics.liveview_frames,
        });
        self.last_health_refresh = now;
        self.health = Some(health);
    }

    fn record_frame(&mut self) {
        self.frames_in_window = self.frames_in_window.saturating_add(1);
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.frame_window_started);
        if elapsed >= Duration::from_secs(1) {
            self.rates.update_hz = self.frames_in_window as f64 / elapsed.as_secs_f64();
            self.frames_in_window = 0;
            self.frame_window_started = now;
        }
    }

    fn apply_trace(&mut self, trace: TraceSnapshot) -> bool {
        if self
            .trace_instance_id
            .as_deref()
            .is_some_and(|instance_id| instance_id != trace.instance_id)
        {
            self.trace_cursor = 0;
            self.trace_instance_id = Some(trace.instance_id);
            return false;
        }
        self.trace_instance_id = Some(trace.instance_id);
        for event in trace.events {
            let kind = if event.is_error() {
                LogKind::Error
            } else {
                LogKind::Info
            };
            let mut line = event.display_line();
            if let Some(error) = event.error.as_deref() {
                line.push_str(": ");
                line.push_str(error);
            }
            self.log(kind, line);
        }
        self.trace_cursor = trace.cursor;
        true
    }
}

struct SharedSurface {
    registry: ActionRegistry,
    client: ControlClient,
    tx: mpsc::Sender<RuntimeEvent>,
    latest: Mutex<Option<CameraSnapshot>>,
    quit: AtomicBool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let client = ControlClient::new(args.control.clone());
    let catalog = client
        .action_catalog()
        .context("fetch simulator action catalog")?;
    let registry = ActionRegistry::from_catalog(catalog);
    registry.parity_report()?;

    let (tx, rx) = mpsc::channel::<RuntimeEvent>();
    let shared = Arc::new(SharedSurface {
        registry: registry.clone(),
        client: client.clone(),
        tx,
        latest: Mutex::new(None),
        quit: AtomicBool::new(false),
    });
    let listen_addr = start_http_surface(&args.listen, Arc::clone(&shared))
        .with_context(|| format!("bind TUI HTTP listener {}", args.listen))?;
    let callback_url = args
        .callback_url
        .unwrap_or_else(|| callback_url_for(listen_addr));

    let mut app = App::new(
        registry,
        client.clone(),
        ConsoleStyle::new(args.theme, args.glyphs),
        listen_addr,
        callback_url.clone(),
    );
    app.log(
        LogKind::Info,
        format!("tui http listening on http://{listen_addr}"),
    );

    match client.health() {
        Ok(health) => {
            app.log(
                LogKind::Info,
                format!(
                    "attached to {} {}",
                    health.profile.as_str(),
                    health.connection.as_str()
                ),
            );
            app.set_health(health);
        }
        Err(e) => app.log(LogKind::Error, format!("health fetch failed: {e}")),
    }
    match client.state() {
        Ok(state) => {
            *shared.latest.lock().unwrap() = Some(state.clone());
            app.set_state(state);
        }
        Err(e) => app.log(LogKind::Error, format!("initial state fetch failed: {e}")),
    }
    refresh_trace(&mut app);
    if !args.no_subscribe {
        match client.subscribe_callback(&callback_url) {
            Ok(_) => app.log(LogKind::Info, format!("subscribed callback {callback_url}")),
            Err(e) => app.log(LogKind::Error, format!("callback subscribe failed: {e}")),
        }
    }

    if args.headless {
        run_headless(app, rx, shared)
    } else {
        run_tui(app, rx, shared)
    }
}

fn run_headless(
    mut app: App,
    rx: mpsc::Receiver<RuntimeEvent>,
    shared: Arc<SharedSurface>,
) -> Result<()> {
    println!(
        "{}",
        serde_json::json!({
            "ok": true,
            "mode": "headless",
            "theme": app.style.theme_name.as_str(),
            "glyphs": app.style.glyph_mode.as_str(),
            "listen": format!("http://{}", app.listen_addr),
            "callback_url": app.callback_url,
            "actions": app.registry.descriptors(),
        })
    );
    while !app.quit && !shared.quit.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(event) => apply_runtime_event(&mut app, event),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn run_tui(
    mut app: App,
    rx: mpsc::Receiver<RuntimeEvent>,
    shared: Arc<SharedSurface>,
) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let stdout = std::io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut dirty = true;
    let mut next_frame = Instant::now();

    while !app.quit && !shared.quit.load(Ordering::Relaxed) {
        dirty |= drain_events(&mut app, &rx);

        if app.last_health_refresh.elapsed() >= HEALTH_REFRESH_INTERVAL {
            refresh_health(&mut app);
            dirty = true;
        }

        if dirty && Instant::now() >= next_frame {
            terminal.draw(|frame| render(frame, &app))?;
            app.record_frame();
            dirty = false;
            next_frame = Instant::now() + MAX_FRAME_INTERVAL;
        }

        let now = Instant::now();
        let poll_timeout = if dirty {
            next_frame
                .saturating_duration_since(now)
                .min(Duration::from_millis(100))
        } else {
            HEALTH_REFRESH_INTERVAL
                .saturating_sub(app.last_health_refresh.elapsed())
                .min(Duration::from_millis(100))
        };
        if event::poll(poll_timeout)? {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char(c) => {
                    if let Some(action) = app.registry.by_hotkey(c) {
                        perform_action(&mut app, &shared, action);
                        dirty = true;
                    }
                }
                KeyCode::Esc => {
                    app.quit = true;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn drain_events(app: &mut App, rx: &mpsc::Receiver<RuntimeEvent>) -> bool {
    let mut changed = false;
    loop {
        match rx.try_recv() {
            Ok(event) => {
                apply_runtime_event(app, event);
                changed = true;
            }
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
    }
    changed
}

fn refresh_health(app: &mut App) {
    match app.client.health() {
        Ok(health) => app.set_health(health),
        Err(e) => app.log(LogKind::Error, format!("health refresh failed: {e}")),
    }
    refresh_trace(app);
}

fn refresh_trace(app: &mut App) {
    match app.client.trace(app.trace_cursor) {
        Ok(trace) => {
            if !app.apply_trace(trace) {
                match app.client.trace(0) {
                    Ok(trace) => {
                        app.apply_trace(trace);
                    }
                    Err(e) => app.log(LogKind::Error, format!("trace refresh failed: {e}")),
                }
            }
        }
        Err(e) => app.log(LogKind::Error, format!("trace refresh failed: {e}")),
    }
}

fn apply_runtime_event(app: &mut App, event: RuntimeEvent) {
    match event {
        RuntimeEvent::State(state) => app.set_state(state),
        RuntimeEvent::Action(msg) => app.log(LogKind::Action, msg),
        RuntimeEvent::Error(msg) => app.log(LogKind::Error, msg),
        RuntimeEvent::Quit => app.quit = true,
    }
}

fn perform_action(app: &mut App, shared: &Arc<SharedSurface>, action: Action) {
    if action.is_quit() {
        shared.quit.store(true, Ordering::Relaxed);
        app.quit = true;
        return;
    }
    let result = match &action.kind {
        ActionKind::Manifest { body } => app.client.invoke_action(action.id(), body),
        ActionKind::Patch { body } => app.client.patch_state(body),
        ActionKind::Quit => return,
    };
    match result {
        Ok(_) => {
            app.log(LogKind::Action, format!("sent action {}", action.id()));
            match app.client.state() {
                Ok(state) => {
                    *shared.latest.lock().unwrap() = Some(state.clone());
                    app.set_state(state);
                }
                Err(e) => app.log(LogKind::Error, format!("state refresh failed: {e}")),
            }
        }
        Err(e) => app.log(
            LogKind::Error,
            format!("action {} failed: {e}", action.id()),
        ),
    }
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(app.style.theme.bg)),
        area,
    );

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(12),
            Constraint::Length(3),
        ])
        .split(area);

    render_header(frame, rows[0], app);
    render_body(frame, rows[1], app);
    render_actions(frame, rows[2], app);
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = app.style.theme;
    let glyphs = app.style.glyphs;
    let health = app.health.as_ref();
    let title = Line::from(vec![
        Span::styled(
            format!(" {} ", glyphs.brand),
            Style::default().fg(theme.cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " PTPSIM CYBERDECK ",
            Style::default().fg(theme.cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " CAMERA OPS ",
            Style::default()
                .fg(theme.yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", glyphs.separator),
            Style::default().fg(theme.muted),
        ),
        Span::styled(
            health
                .map(|h| h.profile.as_str())
                .unwrap_or("profile pending"),
            Style::default().fg(theme.text),
        ),
        Span::styled(" @ ", Style::default().fg(theme.muted)),
        Span::styled(
            health
                .map(|h| h.connection.as_str())
                .unwrap_or("connection pending"),
            Style::default().fg(theme.yellow),
        ),
    ]);
    let sub = Line::from(vec![
        Span::styled(
            format!(
                "{} style {}{}{} ",
                glyphs.signal,
                app.style.theme_name.as_str(),
                glyphs.separator,
                app.style.glyph_mode.as_str()
            ),
            Style::default().fg(theme.cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {} control ", glyphs.bullet),
            Style::default().fg(theme.muted),
        ),
        Span::styled(app.client.addr(), Style::default().fg(theme.green)),
        Span::styled(
            format!("  {} callback ", glyphs.bullet),
            Style::default().fg(theme.muted),
        ),
        Span::styled(app.callback_url.as_str(), Style::default().fg(theme.blue)),
    ]);
    frame.render_widget(
        Paragraph::new(vec![title, sub])
            .block(panel_block(
                " uplink ",
                theme,
                theme.muted,
                glyphs,
                Borders::TOP | Borders::BOTTOM,
            ))
            .style(Style::default().bg(theme.panel)),
        area,
    );
}

fn render_body(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(36),
            Constraint::Percentage(30),
        ])
        .split(area);
    render_camera_panel(frame, cols[0], app);
    render_state_panel(frame, cols[1], app);
    render_events_panel(frame, cols[2], app);
}

fn render_camera_panel(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = app.style.theme;
    let glyphs = app.style.glyphs;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Length(5),
            Constraint::Min(5),
        ])
        .split(area);

    let phase = display_phase(&app.snapshot.phase);
    let phase_color = phase_color(&app.snapshot.phase, theme);
    let phase_lines = vec![
        Line::from(vec![
            Span::styled("PHASE", Style::default().fg(theme.muted)),
            Span::raw("  "),
            Span::styled(
                phase,
                Style::default()
                    .fg(phase_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("SESSION ", Style::default().fg(theme.muted)),
            Span::styled(
                if app.snapshot.session_open {
                    format!("{} OPEN", glyphs.online)
                } else {
                    format!("{} CLOSED", glyphs.offline)
                },
                Style::default()
                    .fg(if app.snapshot.session_open {
                        theme.green
                    } else {
                        theme.red
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("LV FPS  ", Style::default().fg(theme.muted)),
            Span::styled(
                if matches!(app.snapshot.phase.as_str(), "liveView" | "streaming") {
                    format!("{:.1}", app.rates.liveview_fps)
                } else {
                    "inactive".to_string()
                },
                Style::default().fg(theme.green),
            ),
        ]),
        Line::from(vec![
            Span::styled("MEDIA   ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("{} objects", app.snapshot.media.objects),
                Style::default().fg(theme.yellow),
            ),
        ]),
        Line::from(vec![
            Span::styled("STD Q   ", Style::default().fg(theme.muted)),
            Span::styled(
                queue_text(app.snapshot.transfer_queues.standard.as_ref()),
                Style::default().fg(theme.yellow),
            ),
        ]),
        Line::from(vec![
            Span::styled("CAM Q   ", Style::default().fg(theme.muted)),
            Span::styled(
                queue_text(app.snapshot.transfer_queues.camera_initiated.as_ref()),
                Style::default().fg(theme.magenta),
            ),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(phase_lines)
            .alignment(Alignment::Center)
            .block(panel_block(
                " camera core ",
                theme,
                theme.muted,
                glyphs,
                Borders::TOP,
            ))
            .style(Style::default().bg(theme.panel)),
        rows[0],
    );

    let ratio = phase_ratio(&app.snapshot.phase);
    frame.render_widget(
        Gauge::default()
            .block(panel_block(
                " phase charge ",
                theme,
                theme.muted,
                glyphs,
                Borders::TOP,
            ))
            .gauge_style(
                Style::default()
                    .fg(phase_color)
                    .bg(theme.panel_hi)
                    .add_modifier(Modifier::BOLD),
            )
            .label(format!("{} {:.0}% ACTIVE", glyphs.signal, ratio * 100.0))
            .ratio(ratio),
        rows[1],
    );

    let health = app.health.as_ref();
    let metrics = health.map(|h| &h.metrics);
    let health_lines = vec![
        line_kv_color(
            "instance",
            health.map(|h| h.instance_id.clone()),
            theme,
            glyphs,
            theme.green,
        ),
        line_kv_color(
            "command",
            health.map(|h| h.command_bind.clone()),
            theme,
            glyphs,
            theme.text,
        ),
        line_kv_color(
            "sessions",
            health.map(|h| h.sessions.to_string()),
            theme,
            glyphs,
            theme.yellow,
        ),
        line_kv_color(
            "mem alloc",
            metrics.map(|m| format_bytes(m.memory_allocated_bytes)),
            theme,
            glyphs,
            theme.cyan,
        ),
        line_kv_color(
            "bytes xfer",
            metrics.map(|m| format_bytes(m.bytes_transferred)),
            theme,
            glyphs,
            theme.yellow,
        ),
        line_kv_color(
            "rate",
            Some(format_rate(app.rates.transfer_bps)),
            theme,
            glyphs,
            theme.yellow,
        ),
        line_kv_color(
            "uptime",
            metrics.map(|m| format_duration_ms(m.uptime_ms)),
            theme,
            glyphs,
            theme.green,
        ),
        line_kv_color(
            "idle",
            metrics.map(|m| format_duration_ms(m.idle_ms)),
            theme,
            glyphs,
            theme.green,
        ),
        line_kv_color(
            "update",
            Some(format!(
                "{:.1} Hz / {:.0}",
                app.rates.update_hz, MAX_UPDATE_HZ
            )),
            theme,
            glyphs,
            theme.cyan,
        ),
        line_kv_color(
            "listen",
            Some(app.listen_addr.to_string()),
            theme,
            glyphs,
            theme.blue,
        ),
    ];
    frame.render_widget(
        Paragraph::new(health_lines)
            .block(panel_block(
                " telemetry ",
                theme,
                theme.muted,
                glyphs,
                Borders::TOP,
            ))
            .style(Style::default().bg(theme.panel)),
        rows[2],
    );
}

fn render_state_panel(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = app.style.theme;
    let glyphs = app.style.glyphs;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(10), Constraint::Min(8)])
        .split(area);
    let status_lines = vec![
        line_kv_color(
            "profile",
            app.health.as_ref().map(|h| h.profile.clone()),
            theme,
            glyphs,
            theme.green,
        ),
        line_kv_color(
            "connection",
            app.health.as_ref().map(|h| h.connection.clone()),
            theme,
            glyphs,
            theme.green,
        ),
        line_kv(
            "media root",
            app.health.as_ref().map(|h| h.media_root.clone()),
            theme,
            glyphs,
        ),
        line_kv_color(
            "props",
            Some(app.snapshot.props.len().to_string()),
            theme,
            glyphs,
            theme.yellow,
        ),
        line_kv_color(
            "crate",
            Some(format!(
                "{} {}",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            )),
            theme,
            glyphs,
            theme.magenta,
        ),
        line_kv_color(
            "rustc",
            Some(RUSTC_VERSION.to_string()),
            theme,
            glyphs,
            theme.cyan,
        ),
        line_kv_color(
            "target",
            Some(format!(
                "{}-{}",
                std::env::consts::ARCH,
                std::env::consts::OS
            )),
            theme,
            glyphs,
            theme.blue,
        ),
        line_kv_color(
            "deps",
            Some(format!(
                "ratatui {RATATUI_VERSION}, crossterm {CROSSTERM_VERSION}"
            )),
            theme,
            glyphs,
            theme.magenta,
        ),
    ];
    frame.render_widget(
        Paragraph::new(status_lines)
            .block(panel_block(
                " runtime ",
                theme,
                theme.muted,
                glyphs,
                Borders::TOP,
            ))
            .style(Style::default().bg(theme.panel))
            .wrap(Wrap { trim: true }),
        rows[0],
    );

    let prop_lines = if app.snapshot.props.is_empty() {
        vec![Line::from(Span::styled(
            "No properties in current snapshot",
            Style::default().fg(theme.muted),
        ))]
    } else {
        app.snapshot
            .props
            .iter()
            .take(14)
            .map(|(key, value)| {
                let label = app
                    .snapshot
                    .property_labels
                    .get(key)
                    .map(String::as_str)
                    .unwrap_or("property");
                Line::from(vec![
                    Span::styled(
                        format!("{label} ({key}) "),
                        Style::default().fg(theme.cyan).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(value_text(value), Style::default().fg(theme.text)),
                ])
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(prop_lines)
            .block(panel_block(
                " exposure matrix ",
                theme,
                theme.muted,
                glyphs,
                Borders::TOP,
            ))
            .style(Style::default().bg(theme.panel))
            .wrap(Wrap { trim: true }),
        rows[1],
    );
}

fn render_events_panel(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = app.style.theme;
    let glyphs = app.style.glyphs;
    let items = app
        .events
        .iter()
        .take(area.height.saturating_sub(2) as usize)
        .map(|event| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    event_prefix(event.kind, glyphs),
                    event_style(event.kind, theme),
                ),
                Span::raw(" "),
                Span::styled(event.text.as_str(), Style::default().fg(theme.text)),
            ]))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items)
            .block(panel_block(
                " signal log ",
                theme,
                theme.muted,
                glyphs,
                Borders::TOP,
            ))
            .style(Style::default().bg(theme.panel)),
        area,
    );
}

fn render_actions(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = app.style.theme;
    let glyphs = app.style.glyphs;
    let mut spans = Vec::new();
    for action in app.registry.actions() {
        if let Some(key) = action.descriptor.hotkey {
            spans.push(Span::styled(
                format!(
                    " {}{}{} ",
                    glyphs.key_left,
                    key.to_ascii_uppercase(),
                    glyphs.key_right
                ),
                Style::default()
                    .fg(Color::Black)
                    .bg(if action.is_quit() {
                        theme.red
                    } else {
                        theme.cyan
                    })
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!(" {}  ", action.descriptor.label),
                Style::default().fg(theme.text),
            ));
        }
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .alignment(Alignment::Center)
            .block(panel_block(
                " controls // GET /actions ",
                theme,
                theme.muted,
                glyphs,
                Borders::TOP,
            ))
            .style(Style::default().bg(theme.panel)),
        area,
    );
}

fn queue_text(queue: Option<&QueueSnapshot>) -> String {
    queue.map_or_else(
        || "not configured".to_string(),
        |queue| {
            format!(
                "{}q {}done {}total",
                queue.queued, queue.completed, queue.total
            )
        },
    )
}

fn panel_block<'a>(
    title: &'a str,
    theme: Theme,
    color: Color,
    glyphs: Glyphs,
    borders: Borders,
) -> Block<'a> {
    Block::default()
        .title(title)
        .borders(borders)
        .border_type(glyphs.border)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(theme.panel).fg(theme.text))
}

fn line_kv(key: &str, value: Option<String>, theme: Theme, glyphs: Glyphs) -> Line<'static> {
    line_kv_color(key, value, theme, glyphs, theme.text)
}

fn line_kv_color(
    key: &str,
    value: Option<String>,
    theme: Theme,
    glyphs: Glyphs,
    value_color: Color,
) -> Line<'static> {
    let value_style = if value_color == theme.text {
        Style::default().fg(value_color)
    } else {
        Style::default()
            .fg(value_color)
            .add_modifier(Modifier::BOLD)
    };
    Line::from(vec![
        Span::styled(format!("{key:<11}"), Style::default().fg(theme.muted)),
        Span::styled(
            format!("{} ", glyphs.kv_sep),
            Style::default().fg(theme.muted),
        ),
        Span::styled(value.unwrap_or_else(|| "pending".to_string()), value_style),
    ])
}

fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = UNITS[0];
    for candidate in UNITS.iter().skip(1) {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = candidate;
    }
    if unit == "B" {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {unit}")
    }
}

fn format_rate(bytes_per_second: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_second.max(0.0) as u64))
}

fn format_duration_ms(ms: u64) -> String {
    let seconds = ms / 1000;
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m {secs:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {secs:02}s")
    } else {
        format!("{secs}s")
    }
}

fn display_phase(phase: &str) -> &str {
    match phase {
        "sessionOpen" => "SESSION OPEN",
        "imageImport" => "IMAGE IMPORT",
        "liveView" => "LIVE VIEW",
        "streaming" => "STREAMING",
        "closed" => "CLOSED",
        "disconnected" => "DISCONNECTED",
        "" => "UNKNOWN",
        other => other,
    }
}

fn phase_color(phase: &str, theme: Theme) -> Color {
    match phase {
        "streaming" => theme.green,
        "liveView" => theme.cyan,
        "imageImport" => theme.yellow,
        "sessionOpen" => theme.blue,
        "closed" | "disconnected" => theme.red,
        _ => theme.magenta,
    }
}

fn phase_ratio(phase: &str) -> f64 {
    match phase {
        "disconnected" | "" => 0.12,
        "closed" => 0.22,
        "sessionOpen" => 0.42,
        "imageImport" => 0.62,
        "liveView" => 0.78,
        "streaming" => 0.96,
        _ => 0.35,
    }
}

fn event_prefix(kind: LogKind, glyphs: Glyphs) -> &'static str {
    match kind {
        LogKind::Info => glyphs.event_info,
        LogKind::State => glyphs.event_state,
        LogKind::Action => glyphs.event_action,
        LogKind::Error => glyphs.event_error,
    }
}

fn event_style(kind: LogKind, theme: Theme) -> Style {
    let color = match kind {
        LogKind::Info => theme.blue,
        LogKind::State => theme.green,
        LogKind::Action => theme.yellow,
        LogKind::Error => theme.red,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(std::io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), Show, LeaveAlternateScreen);
    }
}

fn start_http_surface(addr: &str, shared: Arc<SharedSurface>) -> Result<SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    thread::spawn(move || {
        for stream in listener.incoming() {
            if shared.quit.load(Ordering::Relaxed) {
                break;
            }
            match stream {
                Ok(mut stream) => {
                    if let Err(e) = handle_http(&mut stream, &shared) {
                        let _ = write_json(
                            &mut stream,
                            "500 Internal Server Error",
                            &serde_json::json!({ "error": e.to_string() }).to_string(),
                        );
                    }
                }
                Err(e) => {
                    let _ = shared
                        .tx
                        .send(RuntimeEvent::Error(format!("http accept failed: {e}")));
                }
            }
        }
    });
    Ok(local)
}

fn handle_http(stream: &mut TcpStream, shared: &Arc<SharedSurface>) -> Result<()> {
    let req = read_request(stream)?;
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/healthz") => write_json(
            stream,
            "200 OK",
            &serde_json::json!({
                "ok": true,
                "surface": "camera-sim-tui",
                "actions": shared.registry.actions().len(),
            })
            .to_string(),
        )?,
        ("GET", "/actions") => write_json(stream, "200 OK", &shared.registry.actions_json())?,
        ("GET", "/operator/actions") => {
            write_json(stream, "200 OK", &shared.registry.operator_actions_json())?
        }
        ("GET", "/state") => {
            let latest = shared.latest.lock().unwrap().clone();
            write_json(
                stream,
                "200 OK",
                &serde_json::json!({ "state": latest }).to_string(),
            )?;
        }
        ("POST", "/state") => {
            let state: CameraSnapshot =
                serde_json::from_slice(&req.body).context("parse pushed state JSON")?;
            *shared.latest.lock().unwrap() = Some(state.clone());
            let _ = shared.tx.send(RuntimeEvent::State(state));
            write_json(stream, "200 OK", r#"{"ok":true}"#)?;
        }
        _ if req.method == "POST" => match shared.registry.by_http_path("POST", &req.path) {
            Some(action) => {
                dispatch_http_action(action, shared, Some(&req.body))?;
                write_json(
                    stream,
                    "200 OK",
                    &serde_json::json!({ "ok": true }).to_string(),
                )?;
            }
            None => write_json(stream, "404 Not Found", r#"{"error":"not found"}"#)?,
        },
        _ => write_json(stream, "404 Not Found", r#"{"error":"not found"}"#)?,
    }
    Ok(())
}

fn dispatch_http_action(
    action: Action,
    shared: &Arc<SharedSurface>,
    request_body: Option<&[u8]>,
) -> Result<()> {
    match &action.kind {
        ActionKind::Quit => {
            shared.quit.store(true, Ordering::Relaxed);
            let _ = shared.tx.send(RuntimeEvent::Quit);
        }
        ActionKind::Patch { body } => {
            shared.client.patch_state(body)?;
            let state = shared.client.state()?;
            *shared.latest.lock().unwrap() = Some(state.clone());
            let _ = shared
                .tx
                .send(RuntimeEvent::Action(format!("http action {}", action.id())));
            let _ = shared.tx.send(RuntimeEvent::State(state));
        }
        ActionKind::Manifest { body } => {
            let body = request_body
                .filter(|body| !body.is_empty())
                .map(std::str::from_utf8)
                .transpose()
                .context("action proxy body is not UTF-8")?
                .unwrap_or(body);
            shared.client.invoke_action(action.id(), body)?;
            let state = shared.client.state()?;
            *shared.latest.lock().unwrap() = Some(state.clone());
            let _ = shared
                .tx
                .send(RuntimeEvent::Action(format!("http action {}", action.id())));
            let _ = shared.tx.send(RuntimeEvent::State(state));
        }
    }
    Ok(())
}

struct LocalRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<LocalRequest> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            anyhow::bail!("client closed before full request");
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 256 * 1024 {
            anyhow::bail!("request too large");
        }
        if let Some(req) = parse_request_if_complete(&buf)? {
            return Ok(req);
        }
    }
}

fn parse_request_if_complete(buf: &[u8]) -> Result<Option<LocalRequest>> {
    let s = std::str::from_utf8(buf).context("request is not UTF-8")?;
    let Some(split) = s.find("\r\n\r\n") else {
        return Ok(None);
    };
    let headers = &s[..split];
    let mut lines = headers.lines();
    let line = lines.next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find_map(|(name, value)| {
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>())
        })
        .transpose()
        .context("invalid Content-Length")?
        .unwrap_or(0);
    let body_start = split + 4;
    if buf.len() < body_start + content_length {
        return Ok(None);
    }
    Ok(Some(LocalRequest {
        method,
        path,
        body: buf[body_start..body_start + content_length].to_vec(),
    }))
}

fn write_json(stream: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new(
            ActionRegistry::core(),
            ControlClient::new("http://127.0.0.1:1"),
            ConsoleStyle::new(ThemeName::Cyberpunk, GlyphMode::Unicode),
            "127.0.0.1:0".parse().unwrap(),
            "http://127.0.0.1:1/state".into(),
        )
    }

    #[test]
    fn cyberpunk_unicode_is_the_default_console_style() {
        let args = Args::parse_from(["camera-sim-tui"]);
        assert!(matches!(args.theme, ThemeName::Cyberpunk));
        assert!(matches!(args.glyphs, GlyphMode::Unicode));
        assert_eq!(args.theme.as_str(), "cyberpunk");
        assert_eq!(args.glyphs.as_str(), "unicode");
    }

    #[test]
    fn cyberpunk_theme_stays_black_and_acid() {
        let theme = ThemeName::Cyberpunk.theme();
        assert_eq!(theme.bg, Color::Black);
        assert_eq!(theme.panel, Color::Black);
        assert_eq!(theme.cyan, Color::LightCyan);
        assert_eq!(theme.green, Color::Green);
        assert_eq!(theme.yellow, Color::Yellow);
    }

    #[test]
    fn unicode_glyphs_use_hard_terminal_symbols() {
        let glyphs = GlyphMode::Unicode.glyphs();
        assert_eq!(glyphs.border, BorderType::LightDoubleDashed);
        assert_eq!(glyphs.key_left, "⟦");
        assert_eq!(glyphs.brand, "◢");
        assert_eq!(glyphs.event_action, "▶");
    }

    #[test]
    fn queue_text_is_compact_and_distinguishes_absent_queues() {
        let queue = QueueSnapshot {
            queued: 2,
            completed: 1,
            total: 3,
        };
        assert_eq!(queue_text(Some(&queue)), "2q 1done 3total");
        assert_eq!(queue_text(None), "not configured");
    }

    #[test]
    fn trace_cursor_resets_when_service_instance_changes() {
        let mut app = test_app();
        assert!(app.apply_trace(TraceSnapshot {
            instance_id: "first".into(),
            cursor: 42,
            events: Vec::new(),
        }));
        assert_eq!(app.trace_cursor, 42);

        assert!(!app.apply_trace(TraceSnapshot {
            instance_id: "second".into(),
            cursor: 3,
            events: Vec::new(),
        }));
        assert_eq!(app.trace_cursor, 0);
        assert_eq!(app.trace_instance_id.as_deref(), Some("second"));

        assert!(app.apply_trace(TraceSnapshot {
            instance_id: "second".into(),
            cursor: 3,
            events: Vec::new(),
        }));
        assert_eq!(app.trace_cursor, 3);
    }
}
