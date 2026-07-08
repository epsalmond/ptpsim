use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use camera_sim_tui::{
    callback_url_for, Action, ActionKind, ActionRegistry, CameraSnapshot, ControlClient,
    HealthSnapshot,
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
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

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
    #[arg(long, value_enum, default_value_t = ThemeName::Neon)]
    theme: ThemeName,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ThemeName {
    Neon,
    Mono,
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
    fn theme(self) -> Theme {
        match self {
            ThemeName::Neon => Theme {
                bg: Color::Rgb(5, 7, 14),
                panel: Color::Rgb(13, 18, 34),
                panel_hi: Color::Rgb(22, 30, 54),
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

struct App {
    registry: ActionRegistry,
    client: ControlClient,
    theme: Theme,
    listen_addr: SocketAddr,
    callback_url: String,
    health: Option<HealthSnapshot>,
    snapshot: CameraSnapshot,
    events: VecDeque<EventLine>,
    quit: bool,
    tick: u64,
}

impl App {
    fn new(
        registry: ActionRegistry,
        client: ControlClient,
        theme: Theme,
        listen_addr: SocketAddr,
        callback_url: String,
    ) -> Self {
        Self {
            registry,
            client,
            theme,
            listen_addr,
            callback_url,
            health: None,
            snapshot: CameraSnapshot::default(),
            events: VecDeque::new(),
            quit: false,
            tick: 0,
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
        self.snapshot = state;
        self.log(
            LogKind::State,
            format!("state push: phase={phase} objects={objects}"),
        );
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
    let registry = ActionRegistry::core();
    registry.parity_report()?;

    let client = ControlClient::new(args.control.clone());
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
        args.theme.theme(),
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
            app.health = Some(health);
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

    while !app.quit && !shared.quit.load(Ordering::Relaxed) {
        drain_events(&mut app, &rx);
        app.tick = app.tick.wrapping_add(1);
        terminal.draw(|frame| render(frame, &app))?;

        if event::poll(Duration::from_millis(80))? {
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

fn drain_events(app: &mut App, rx: &mpsc::Receiver<RuntimeEvent>) {
    loop {
        match rx.try_recv() {
            Ok(event) => apply_runtime_event(app, event),
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => break,
        }
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
    let Some(body) = action.patch_body() else {
        return;
    };
    match app.client.patch_state(body) {
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
        Block::default().style(Style::default().bg(app.theme.bg)),
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
    let theme = app.theme;
    let health = app.health.as_ref();
    let title = Line::from(vec![
        Span::styled(
            " PTPSIM ",
            Style::default()
                .fg(theme.bg)
                .bg(theme.cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " CAMERA OPERATOR ",
            Style::default()
                .fg(theme.magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("// ", Style::default().fg(theme.muted)),
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
        Span::styled("control ", Style::default().fg(theme.muted)),
        Span::styled(app.client.addr(), Style::default().fg(theme.green)),
        Span::styled("  callback ", Style::default().fg(theme.muted)),
        Span::styled(app.callback_url.as_str(), Style::default().fg(theme.blue)),
    ]);
    frame.render_widget(
        Paragraph::new(vec![title, sub])
            .block(panel_block(" operator link ", theme, theme.cyan))
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
    let theme = app.theme;
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
        Line::from(""),
        Line::from(vec![
            Span::styled("SESSION ", Style::default().fg(theme.muted)),
            Span::styled(
                if app.snapshot.session_open {
                    "OPEN"
                } else {
                    "CLOSED"
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
            Span::styled("MEDIA   ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("{} objects", app.snapshot.media.objects),
                Style::default().fg(theme.yellow),
            ),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(phase_lines)
            .alignment(Alignment::Center)
            .block(panel_block(" camera face ", theme, phase_color))
            .style(Style::default().bg(theme.panel)),
        rows[0],
    );

    let ratio = phase_ratio(&app.snapshot.phase);
    frame.render_widget(
        Gauge::default()
            .block(panel_block(" state intensity ", theme, theme.magenta))
            .gauge_style(
                Style::default()
                    .fg(phase_color)
                    .bg(theme.panel_hi)
                    .add_modifier(Modifier::BOLD),
            )
            .label(format!("{:.0}% active", ratio * 100.0))
            .ratio(ratio),
        rows[1],
    );

    let health = app.health.as_ref();
    let health_lines = vec![
        line_kv("instance", health.map(|h| h.instance_id.clone()), theme),
        line_kv("command", health.map(|h| h.command_bind.clone()), theme),
        line_kv("sessions", health.map(|h| h.sessions.to_string()), theme),
        line_kv("listen", Some(app.listen_addr.to_string()), theme),
    ];
    frame.render_widget(
        Paragraph::new(health_lines)
            .block(panel_block(" health ", theme, theme.green))
            .style(Style::default().bg(theme.panel)),
        rows[2],
    );
}

fn render_state_panel(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = app.theme;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(8)])
        .split(area);
    let status_lines = vec![
        line_kv(
            "profile",
            app.health.as_ref().map(|h| h.profile.clone()),
            theme,
        ),
        line_kv(
            "connection",
            app.health.as_ref().map(|h| h.connection.clone()),
            theme,
        ),
        line_kv(
            "media root",
            app.health.as_ref().map(|h| h.media_root.clone()),
            theme,
        ),
        line_kv("props", Some(app.snapshot.props.len().to_string()), theme),
    ];
    frame.render_widget(
        Paragraph::new(status_lines)
            .block(panel_block(" simulator ", theme, theme.blue))
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
                Line::from(vec![
                    Span::styled(
                        format!("{key:<8}"),
                        Style::default().fg(theme.cyan).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(value_text(value), Style::default().fg(theme.text)),
                ])
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(prop_lines)
            .block(panel_block(" selected properties ", theme, theme.yellow))
            .style(Style::default().bg(theme.panel))
            .wrap(Wrap { trim: true }),
        rows[1],
    );
}

fn render_events_panel(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = app.theme;
    let items = app
        .events
        .iter()
        .take(area.height.saturating_sub(2) as usize)
        .map(|event| {
            ListItem::new(Line::from(vec![
                Span::styled(event_prefix(event.kind), event_style(event.kind, theme)),
                Span::raw(" "),
                Span::styled(event.text.as_str(), Style::default().fg(theme.text)),
            ]))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items)
            .block(panel_block(" recent events ", theme, theme.magenta))
            .style(Style::default().bg(theme.panel)),
        area,
    );
}

fn render_actions(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let theme = app.theme;
    let mut spans = Vec::new();
    for action in app.registry.actions() {
        if let Some(key) = action.descriptor.hotkey {
            spans.push(Span::styled(
                format!(" {} ", key.to_ascii_uppercase()),
                Style::default()
                    .fg(theme.bg)
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
                " hotkeys / HTTP parity at GET /actions ",
                theme,
                theme.green,
            ))
            .style(Style::default().bg(theme.panel)),
        area,
    );
}

fn panel_block<'a>(title: &'a str, theme: Theme, color: Color) -> Block<'a> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(theme.panel).fg(theme.text))
}

fn line_kv(key: &str, value: Option<String>, theme: Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<11}"), Style::default().fg(theme.muted)),
        Span::styled(
            value.unwrap_or_else(|| "pending".to_string()),
            Style::default().fg(theme.text),
        ),
    ])
}

fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
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

fn event_prefix(kind: LogKind) -> &'static str {
    match kind {
        LogKind::Info => "INFO",
        LogKind::State => "PUSH",
        LogKind::Action => "ACT ",
        LogKind::Error => "ERR ",
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
                dispatch_http_action(action, shared)?;
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

fn dispatch_http_action(action: Action, shared: &Arc<SharedSurface>) -> Result<()> {
    match action.kind {
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
