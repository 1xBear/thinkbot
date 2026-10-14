//! thinkbot — a small ratatui control panel with a yt-dlp front-end.

use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Gauge, List, ListState, Paragraph, Wrap},
    DefaultTerminal, Frame,
};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    env, fs,
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TICK: Duration = Duration::from_millis(100);
const SYSTEM_REFRESH: Duration = Duration::from_secs(2);
const MAX_LOG_LINES: usize = 200;
const ON_ACCENT: Color = Color::Black;

// ---------------------------------------------------------
// Small helpers
// ---------------------------------------------------------

/// Wrap `idx + delta` into `0..len`.
fn wrap(idx: usize, len: usize, delta: i32) -> usize {
    (idx as i32 + delta).rem_euclid(len as i32) as usize
}

fn home() -> Option<String> {
    env::var("HOME").ok()
}

fn expand_tilde(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), home()) {
        (Some(rest), Some(h)) => PathBuf::from(h).join(rest),
        _ => PathBuf::from(path),
    }
}

fn read(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Last `n` characters of `s` (keeps the text cursor visible in narrow inputs).
fn tail_chars(s: &str, n: usize) -> String {
    s.chars().skip(s.chars().count().saturating_sub(n)).collect()
}

fn vertical(code: KeyCode) -> Option<i32> {
    match code {
        KeyCode::Down | KeyCode::Char('j') => Some(1),
        KeyCode::Up | KeyCode::Char('k') => Some(-1),
        _ => None,
    }
}

fn is_back(code: KeyCode) -> bool {
    matches!(code, KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h'))
}

/// Apply a key to a text buffer. Returns true if the key was consumed.
fn edit_text(buf: &mut String, key: KeyEvent) -> bool {
    match (key.code, key.modifiers.contains(KeyModifiers::CONTROL)) {
        (KeyCode::Char('u'), true) => buf.clear(),
        (KeyCode::Char(c), false) => buf.push(c),
        (KeyCode::Backspace, _) => {
            buf.pop();
        }
        _ => return false,
    }
    true
}

// ---------------------------------------------------------
// Themes
// ---------------------------------------------------------

const fn hex(c: u32) -> Color {
    Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

#[derive(Clone, Copy)]
struct Palette {
    primary: Color,
    secondary: Color,
    highlight: Color,
    text: Color,
    dim: Color,
    surface: Color,
    success: Color,
    danger: Color,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Theme {
    CatppuccinMocha,
    TokyoNight,
    Nord,
    Monokai,
    Cyberpunk,
    GruvboxDark,
}

impl Theme {
    const ALL: [Theme; 6] = [
        Theme::CatppuccinMocha,
        Theme::TokyoNight,
        Theme::Nord,
        Theme::Monokai,
        Theme::Cyberpunk,
        Theme::GruvboxDark,
    ];

    fn name(self) -> &'static str {
        match self {
            Theme::CatppuccinMocha => "Catppuccin Mocha",
            Theme::TokyoNight => "Tokyo Night",
            Theme::Nord => "Nord Ice",
            Theme::Monokai => "Monokai Pro",
            Theme::Cyberpunk => "Cyberpunk Neon",
            Theme::GruvboxDark => "Gruvbox Dark",
        }
    }

    fn cycle(self, delta: i32) -> Self {
        let i = Self::ALL.iter().position(|&t| t == self).unwrap_or(0);
        Self::ALL[wrap(i, Self::ALL.len(), delta)]
    }

    fn palette(self) -> Palette {
        // primary, secondary, highlight, text, dim, surface, success, danger
        let c = match self {
            Theme::CatppuccinMocha => [0x89b4fa, 0xcba6f7, 0xb4befe, 0xcdd6f4, 0x585b70, 0x313244, 0xa6e3a1, 0xf38ba8],
            Theme::TokyoNight => [0x7aa2f7, 0xbb9af7, 0xe0af68, 0xc0caf5, 0x414868, 0x24283b, 0x9ece6a, 0xf7768e],
            Theme::Nord => [0x88c0d0, 0x81a1c1, 0xebcb8b, 0xeceff4, 0x4c566a, 0x3b4252, 0xa3be8c, 0xbf616a],
            Theme::Monokai => [0x66d9ef, 0xf92672, 0xe6db74, 0xf8f8f2, 0x75715e, 0x3e3d32, 0xa6e22e, 0xf92672],
            Theme::Cyberpunk => [0x00ffea, 0xff007f, 0xfee715, 0xffffff, 0x505064, 0x1a1a2e, 0x39ff14, 0xff3131],
            Theme::GruvboxDark => [0xfe8019, 0xb8bb26, 0xfabd2f, 0xebdbb2, 0x665c54, 0x3c3836, 0xb8bb26, 0xfb4934],
        };
        Palette {
            primary: hex(c[0]),
            secondary: hex(c[1]),
            highlight: hex(c[2]),
            text: hex(c[3]),
            dim: hex(c[4]),
            surface: hex(c[5]),
            success: hex(c[6]),
            danger: hex(c[7]),
        }
    }
}

// ---------------------------------------------------------
// Config
// ---------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Config {
    download_dir: String,
    theme: Theme,
    embed_subtitles: bool,
    concurrent_fragments: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            download_dir: home().map_or_else(|| "/tmp".into(), |h| format!("{h}/Downloads")),
            theme: Theme::GruvboxDark,
            embed_subtitles: false,
            concurrent_fragments: 4,
        }
    }
}

impl Config {
    fn path() -> PathBuf {
        let base = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(home().unwrap_or_else(|| ".".into())).join(".config"));
        base.join("thinkbot").join("config.toml")
    }

    fn load() -> Self {
        let mut cfg: Self = read(Self::path())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        cfg.concurrent_fragments = cfg.concurrent_fragments.clamp(1, 16);
        cfg
    }

    fn save(&self) -> io::Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self).map_err(io::Error::other)?;
        fs::write(path, text)
    }
}

// ---------------------------------------------------------
// Navigation
// ---------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Overview,
    Download,
    Analytics,
    Settings,
    Help,
}

impl Menu {
    const ALL: [Menu; 5] = [Menu::Overview, Menu::Download, Menu::Analytics, Menu::Settings, Menu::Help];

    fn label(self) -> &'static str {
        match self {
            Menu::Overview => "◈ System Overview",
            Menu::Download => "▼ Download Media",
            Menu::Analytics => "▲ Analytics",
            Menu::Settings => "◆ Settings",
            Menu::Help => "? Help",
        }
    }

    /// Overview and Help are static; everything else takes keyboard focus.
    fn is_enterable(self) -> bool {
        !matches!(self, Menu::Overview | Menu::Help)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sidebar,
    Content,
}

// ---------------------------------------------------------
// Downloader
// ---------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Video,
    Audio,
}

impl Kind {
    const ALL: [Kind; 2] = [Kind::Video, Kind::Audio];

    fn label(self) -> &'static str {
        match self {
            Kind::Video => "▤ Video",
            Kind::Audio => "♪ Audio",
        }
    }

    fn formats(self) -> &'static [&'static str] {
        match self {
            Kind::Video => &["mp4", "mkv", "webm", "mov"],
            Kind::Audio => &["mp3", "m4a", "flac", "wav", "opus"],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Url,
    Kind,
    Format,
    Running,
    Done,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum JobState {
    #[default]
    Running,
    Succeeded,
    Failed,
}

/// Output of a running yt-dlp process, shared with the UI thread.
#[derive(Default)]
struct Job {
    lines: Vec<String>,
    percent: f64,
    state: JobState,
}

fn is_progress(line: &str) -> bool {
    line.starts_with("[download]") && line.contains("% of")
}

impl Job {
    fn push(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if is_progress(line) {
            if let Some(pct) = line.split('%').next().and_then(|s| s.rsplit(char::is_whitespace).next()) {
                self.percent = pct.parse().unwrap_or(self.percent);
            }
            // Collapse consecutive progress lines into one.
            if self.lines.last().is_some_and(|l| is_progress(l)) {
                self.lines.pop();
            }
        }
        self.lines.push(line.to_string());
        if self.lines.len() > MAX_LOG_LINES {
            self.lines.remove(0);
        }
    }
}

struct Request {
    url: String,
    kind: Kind,
    format: &'static str,
    dir: PathBuf,
    subtitles: bool,
    threads: u8,
}

fn pump(reader: impl Read + Send + 'static, job: Arc<Mutex<Job>>) -> JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            job.lock().unwrap().push(&line);
        }
    })
}

fn spawn_download(req: Request) -> Arc<Mutex<Job>> {
    let job = Arc::new(Mutex::new(Job::default()));
    let shared = Arc::clone(&job);
    thread::spawn(move || run_download(req, shared));
    job
}

fn run_download(req: Request, job: Arc<Mutex<Job>>) {
    let mut cmd = Command::new("yt-dlp");
    cmd.args(["--newline", "-N"]).arg(req.threads.to_string()).arg("-P").arg(&req.dir);

    match req.kind {
        Kind::Audio => cmd.args(["-x", "--audio-format", req.format]),
        Kind::Video => {
            if req.subtitles {
                cmd.args(["--write-auto-subs", "--sub-langs", "en", "--embed-subs"]);
            }
            cmd.args(["--merge-output-format", req.format])
        }
    };

    // `--` stops a pasted "URL" from ever being parsed as an option.
    cmd.arg("--")
        .arg(&req.url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    job.lock().unwrap().push(&format!("Saving to {}", req.dir.display()));

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let mut j = job.lock().unwrap();
            j.push(&format!("Failed to launch yt-dlp: {e}"));
            j.push("Make sure yt-dlp is installed and on your PATH.");
            j.state = JobState::Failed;
            return;
        }
    };

    let readers = [
        child.stdout.take().map(|o| pump(o, Arc::clone(&job))),
        child.stderr.take().map(|e| pump(e, Arc::clone(&job))),
    ];
    let status = child.wait();
    for handle in readers.into_iter().flatten() {
        let _ = handle.join();
    }

    let mut j = job.lock().unwrap();
    match status {
        Ok(s) if s.success() => {
            j.percent = 100.0;
            j.state = JobState::Succeeded;
        }
        Ok(s) => {
            j.push(&format!("yt-dlp exited with {s}"));
            j.state = JobState::Failed;
        }
        Err(e) => {
            j.push(&format!("Could not wait for yt-dlp: {e}"));
            j.state = JobState::Failed;
        }
    }
}

struct Downloader {
    step: Step,
    url: String,
    kind_idx: usize,
    format_idx: usize,
    job: Arc<Mutex<Job>>,
}

impl Downloader {
    fn new() -> Self {
        Self {
            step: Step::Url,
            url: String::new(),
            kind_idx: 0,
            format_idx: 0,
            job: Arc::default(),
        }
    }

    fn kind(&self) -> Kind {
        Kind::ALL[self.kind_idx]
    }

    fn formats(&self) -> &'static [&'static str] {
        self.kind().formats()
    }

    fn format(&self) -> &'static str {
        self.formats()[self.format_idx]
    }
}

struct HistoryEntry {
    url: String,
    format: &'static str,
    ok: bool,
}

// ---------------------------------------------------------
// Settings
// ---------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Path,
    Theme,
    Subtitles,
    Threads,
    Save,
}

impl Field {
    const ALL: [Field; 5] = [Field::Path, Field::Theme, Field::Subtitles, Field::Threads, Field::Save];
}

struct Settings {
    cfg: Config,
    field: usize,
    /// `Some(buffer)` while the download path is being edited.
    editing: Option<String>,
    status: Option<(String, bool)>,
}

impl Settings {
    fn new() -> Self {
        Self { cfg: Config::load(), field: 0, editing: None, status: None }
    }

    fn field(&self) -> Field {
        Field::ALL[self.field]
    }

    fn move_field(&mut self, delta: i32) {
        self.field = wrap(self.field, Field::ALL.len(), delta);
        self.status = None;
    }

    fn adjust(&mut self, delta: i32) {
        match self.field() {
            Field::Theme => self.cfg.theme = self.cfg.theme.cycle(delta),
            Field::Subtitles => self.cfg.embed_subtitles = !self.cfg.embed_subtitles,
            Field::Threads => {
                self.cfg.concurrent_fragments =
                    (self.cfg.concurrent_fragments as i32 + delta).clamp(1, 16) as u8;
            }
            _ => {}
        }
        self.status = None;
    }

    fn save(&mut self) {
        self.status = Some(match self.cfg.save() {
            Ok(()) => (format!("✔ Saved to {}", Config::path().display()), true),
            Err(e) => (format!("✖ Could not save config: {e}"), false),
        });
    }
}

// ---------------------------------------------------------
// Host metrics
// ---------------------------------------------------------

struct SystemSnapshot {
    os: String,
    host: String,
    kernel: String,
    uptime: String,
    root_fs: String,
    desktop: String,
    load: String,
    mem_used_mib: u64,
    mem_total_mib: u64,
    battery: Option<(u16, String)>,
    temp_c: Option<u16>,
}

fn battery() -> Option<(u16, String)> {
    let dir = fs::read_dir("/sys/class/power_supply")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("BAT")))?;
    let pct = read(dir.join("capacity"))?.parse().ok()?;
    let status = read(dir.join("status")).unwrap_or_else(|| "Unknown".into());
    Some((pct, status))
}

fn cpu_temp() -> Option<u16> {
    let mut zones: Vec<PathBuf> = fs::read_dir("/sys/class/thermal")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("thermal_zone")))
        .collect();
    zones.sort();
    let zone = zones
        .iter()
        .find(|z| read(z.join("type")).as_deref() == Some("x86_pkg_temp"))
        .or(zones.first())?;
    let millideg: u32 = read(zone.join("temp"))?.parse().ok()?;
    Some((millideg / 1000) as u16)
}

fn format_uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    if d > 0 { format!("{d}d {h}h {m}m") } else { format!("{h}h {m}m") }
}

impl SystemSnapshot {
    fn poll() -> Self {
        let os = read("/etc/os-release")
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
            })
            .unwrap_or_else(|| "Linux".into());

        let uptime_secs = read("/proc/uptime")
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
            .unwrap_or(0.0) as u64;

        let root_fs = read("/proc/mounts")
            .and_then(|s| {
                s.lines()
                    .filter_map(|l| {
                        let mut f = l.split_whitespace().skip(1);
                        (f.next()? == "/").then(|| f.next().unwrap_or("?").to_string())
                    })
                    .last()
            })
            .unwrap_or_else(|| "unknown".into());

        let meminfo = read("/proc/meminfo").unwrap_or_default();
        let kib = |key: &str| -> u64 {
            meminfo
                .lines()
                .find_map(|l| l.strip_prefix(key)?.split_whitespace().next()?.parse().ok())
                .unwrap_or(0)
        };
        let (total, avail) = (kib("MemTotal:"), kib("MemAvailable:"));

        let desktop = match (env::var("XDG_CURRENT_DESKTOP"), env::var("XDG_SESSION_TYPE")) {
            (Ok(d), Ok(s)) => format!("{d} ({s})"),
            (Ok(d), Err(_)) => d,
            _ => "unknown".into(),
        };

        Self {
            os: format!("{os} {}", env::consts::ARCH),
            host: read("/proc/sys/kernel/hostname").unwrap_or_else(|| "unknown".into()),
            kernel: read("/proc/sys/kernel/osrelease").unwrap_or_else(|| "unknown".into()),
            uptime: format_uptime(uptime_secs),
            root_fs,
            desktop,
            load: read("/proc/loadavg")
                .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
                .unwrap_or_else(|| "n/a".into()),
            mem_used_mib: total.saturating_sub(avail) / 1024,
            mem_total_mib: total / 1024,
            battery: battery(),
            temp_c: cpu_temp(),
        }
    }
}

// ---------------------------------------------------------
// App
// ---------------------------------------------------------

struct App {
    menu: usize,
    focus: Focus,
    quit: bool,
    dl: Downloader,
    settings: Settings,
    sys: SystemSnapshot,
    last_poll: Instant,
    history: Vec<HistoryEntry>,
}

impl App {
    fn new() -> Self {
        Self {
            menu: 0,
            focus: Focus::Sidebar,
            quit: false,
            dl: Downloader::new(),
            settings: Settings::new(),
            sys: SystemSnapshot::poll(),
            last_poll: Instant::now(),
            history: Vec::new(),
        }
    }

    fn palette(&self) -> Palette {
        self.settings.cfg.theme.palette()
    }

    fn selected(&self) -> Menu {
        Menu::ALL[self.menu]
    }

    fn start_download(&mut self) {
        let cfg = &self.settings.cfg;
        let req = Request {
            url: self.dl.url.trim().to_string(),
            kind: self.dl.kind(),
            format: self.dl.format(),
            dir: expand_tilde(&cfg.download_dir),
            subtitles: cfg.embed_subtitles,
            threads: cfg.concurrent_fragments,
        };
        self.dl.job = spawn_download(req);
        self.dl.step = Step::Running;
    }

    fn reset_download(&mut self) {
        self.dl = Downloader::new();
    }

    /// Periodic housekeeping: refresh metrics and detect finished downloads.
    fn tick(&mut self) {
        if self.last_poll.elapsed() >= SYSTEM_REFRESH {
            self.sys = SystemSnapshot::poll();
            self.last_poll = Instant::now();
        }

        if self.dl.step == Step::Running {
            let state = self.dl.job.lock().unwrap().state;
            if state != JobState::Running {
                self.history.push(HistoryEntry {
                    url: self.dl.url.trim().to_string(),
                    format: self.dl.format(),
                    ok: state == JobState::Succeeded,
                });
                self.dl.step = Step::Done;
            }
        }
    }
}

fn main() -> io::Result<()> {
    // `init` also installs a panic hook, so the terminal is restored on crash.
    let mut terminal = ratatui::init();
    let result = run(&mut terminal);
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal) -> io::Result<()> {
    let mut app = App::new();
    while !app.quit {
        terminal.draw(|f| ui(f, &app))?;

        if event::poll(TICK)? {
            // Drain everything pending so a large paste costs one redraw.
            loop {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        on_key(&mut app, key);
                    }
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        app.tick();
    }
    Ok(())
}

// ---------------------------------------------------------
// Input
// ---------------------------------------------------------

fn on_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.quit = true;
        return;
    }
    match (app.focus, app.selected()) {
        (Focus::Sidebar, _) => sidebar_key(app, key.code),
        (Focus::Content, Menu::Download) => download_key(app, key),
        (Focus::Content, Menu::Settings) => settings_key(app, key),
        (Focus::Content, _) => match key.code {
            KeyCode::Char('q') => app.quit = true,
            c if c == KeyCode::Esc || is_back(c) => app.focus = Focus::Sidebar,
            _ => {}
        },
    }
}

fn sidebar_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') if app.selected().is_enterable() => {
            app.focus = Focus::Content;
        }
        c => {
            if let Some(d) = vertical(c) {
                app.menu = wrap(app.menu, Menu::ALL.len(), d);
            }
        }
    }
}

fn download_key(app: &mut App, key: KeyEvent) {
    let code = key.code;
    match app.dl.step {
        Step::Url => match code {
            KeyCode::Esc => app.focus = Focus::Sidebar,
            KeyCode::Enter if !app.dl.url.trim().is_empty() => app.dl.step = Step::Kind,
            _ => {
                edit_text(&mut app.dl.url, key);
            }
        },
        Step::Kind => match code {
            KeyCode::Esc => app.focus = Focus::Sidebar,
            KeyCode::Enter => {
                app.dl.format_idx = 0;
                app.dl.step = Step::Format;
            }
            c if is_back(c) => app.dl.step = Step::Url,
            c => {
                if let Some(d) = vertical(c) {
                    app.dl.kind_idx = wrap(app.dl.kind_idx, Kind::ALL.len(), d);
                }
            }
        },
        Step::Format => match code {
            KeyCode::Esc => app.focus = Focus::Sidebar,
            KeyCode::Enter => app.start_download(),
            c if is_back(c) => app.dl.step = Step::Kind,
            c => {
                if let Some(d) = vertical(c) {
                    app.dl.format_idx = wrap(app.dl.format_idx, app.dl.formats().len(), d);
                }
            }
        },
        // The download keeps running in the background if you leave.
        Step::Running => {
            if code == KeyCode::Esc {
                app.focus = Focus::Sidebar;
            }
        }
        Step::Done => match code {
            KeyCode::Enter | KeyCode::Char('r') => app.reset_download(),
            c if c == KeyCode::Esc || is_back(c) => app.focus = Focus::Sidebar,
            _ => {}
        },
    }
}

fn settings_key(app: &mut App, key: KeyEvent) {
    let s = &mut app.settings;

    if let Some(buf) = s.editing.as_mut() {
        match key.code {
            KeyCode::Enter => {
                let path = buf.trim().to_string();
                if !path.is_empty() {
                    s.cfg.download_dir = path;
                }
                s.editing = None;
            }
            KeyCode::Esc => s.editing = None,
            _ => {
                edit_text(buf, key);
            }
        }
        return;
    }

    match key.code {
        KeyCode::Esc => app.focus = Focus::Sidebar,
        KeyCode::Down | KeyCode::Char('j') => s.move_field(1),
        KeyCode::Up | KeyCode::Char('k') => s.move_field(-1),
        KeyCode::Left | KeyCode::Char('h') => s.adjust(-1),
        KeyCode::Right | KeyCode::Char('l') => s.adjust(1),
        KeyCode::Enter => match s.field() {
            Field::Path => s.editing = Some(s.cfg.download_dir.clone()),
            Field::Save => s.save(),
            _ => s.adjust(1),
        },
        _ => {}
    }
}

// ---------------------------------------------------------
// Shared widgets
// ---------------------------------------------------------

fn panel<'a>(title: impl Into<Line<'a>>, color: Color) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title.into())
        .border_style(Style::new().fg(color))
}

/// A settings card; the active one gets a marker and the highlight colour.
fn card(title: &str, active: bool, p: &Palette) -> Block<'static> {
    if active {
        panel(format!(" ▶ {title} "), p.highlight)
    } else {
        panel(format!(" {title} "), p.dim)
    }
}

fn val<'a>(text: impl Into<Cow<'a, str>>, color: Color) -> Span<'a> {
    Span::styled(text, Style::new().fg(color))
}

fn kv<'a>(label: &str, value: Span<'a>, p: &Palette) -> Line<'a> {
    let key = Span::styled(format!(" {label:<9}"), Style::new().fg(p.primary).add_modifier(Modifier::BOLD));
    Line::from(vec![key, value])
}

fn gauge(title: String, pct: u16, color: Color, p: &Palette) -> Gauge<'static> {
    Gauge::default()
        .block(panel(title, p.dim))
        .gauge_style(Style::new().fg(color).bg(p.surface))
        .percent(pct.min(100))
        .use_unicode(true)
}

fn render_choices(f: &mut Frame, area: Rect, title: &str, items: Vec<String>, selected: usize, accent: Color, p: &Palette) {
    let list = List::new(items)
        .block(panel(format!(" {title} "), p.dim))
        .highlight_style(Style::new().bg(accent).fg(ON_ACCENT).add_modifier(Modifier::BOLD))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(list, area, &mut ListState::default().with_selected(Some(selected)));
}

// ---------------------------------------------------------
// Rendering
// ---------------------------------------------------------

fn ui(f: &mut Frame, app: &App) {
    let [side, main] = Layout::horizontal([Constraint::Length(26), Constraint::Min(40)]).areas(f.area());
    render_sidebar(f, app, side);
    match app.selected() {
        Menu::Overview => render_overview(f, app, main),
        Menu::Download => render_downloader(f, app, main),
        Menu::Analytics => render_analytics(f, app, main),
        Menu::Settings => render_settings(f, app, main),
        Menu::Help => render_help(f, app, main),
    }
}

fn render_sidebar(f: &mut Frame, app: &App, area: Rect) {
    let p = app.palette();
    let focused = app.focus == Focus::Sidebar;

    let highlight = if focused {
        Style::new().bg(p.primary).fg(ON_ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::new().bg(p.surface).fg(p.text)
    };

    let list = List::new(Menu::ALL.map(Menu::label))
        .style(Style::new().fg(p.text))
        .block(
            panel(" Navigation ", if focused { p.primary } else { p.dim })
                .title_bottom(Line::styled(" q quit ", Style::new().fg(p.dim))),
        )
        .highlight_style(highlight)
        .highlight_symbol("▸ ");

    f.render_stateful_widget(list, area, &mut ListState::default().with_selected(Some(app.menu)));
}

fn render_overview(f: &mut Frame, app: &App, area: Rect) {
    let p = app.palette();
    let sys = &app.sys;

    let frame = panel(" Host Environment ", p.primary);
    let inner = frame.inner(area);
    f.render_widget(frame, area);

    let [ident, gauges, env] =
        Layout::vertical([Constraint::Length(6), Constraint::Length(3), Constraint::Min(4)]).areas(inner);

    // Identity card, two columns
    let card_block = panel(" System ", p.dim);
    let card_inner = card_block.inner(ident);
    f.render_widget(card_block, ident);
    let [left, right] = Layout::horizontal([Constraint::Percentage(50); 2]).areas(card_inner);

    let temp = match sys.temp_c {
        Some(t) => val(format!("{t} °C"), if t > 75 { p.danger } else { p.success }),
        None => val("n/a", p.dim),
    };
    f.render_widget(
        Paragraph::new(vec![
            kv("OS", val(sys.os.as_str(), p.text), &p),
            kv("Host", val(sys.host.as_str(), p.text), &p),
            kv("Kernel", val(sys.kernel.as_str(), p.text), &p),
            kv("Uptime", val(sys.uptime.as_str(), p.success), &p),
        ]),
        left,
    );
    f.render_widget(
        Paragraph::new(vec![
            kv("Desktop", val(sys.desktop.as_str(), p.text), &p),
            kv("Root FS", val(sys.root_fs.as_str(), p.text), &p),
            kv("Load", val(sys.load.as_str(), p.highlight), &p),
            kv("CPU", temp, &p),
        ]),
        right,
    );

    // Gauges
    let [mem_area, bat_area] = Layout::horizontal([Constraint::Percentage(50); 2]).areas(gauges);

    let mem_pct = if sys.mem_total_mib > 0 { (sys.mem_used_mib * 100 / sys.mem_total_mib) as u16 } else { 0 };
    let mem_color = if mem_pct > 85 { p.danger } else { p.primary };
    let mem_title = format!(" Memory · {} / {} MiB ", sys.mem_used_mib, sys.mem_total_mib);
    f.render_widget(gauge(mem_title, mem_pct, mem_color, &p), mem_area);

    match &sys.battery {
        Some((pct, status)) => {
            let color = match *pct {
                0..=19 => p.danger,
                20..=49 => p.highlight,
                _ => p.success,
            };
            f.render_widget(gauge(format!(" Power · {status} "), *pct, color, &p), bat_area);
        }
        None => f.render_widget(
            Paragraph::new(" No battery detected").style(Style::new().fg(p.dim)).block(panel(" Power ", p.dim)),
            bat_area,
        ),
    }

    // Environment
    let row = |label: &'static str, value: Span<'static>| {
        Line::from(vec![
            val("● ", p.success),
            Span::styled(format!("{label:<16}"), Style::new().add_modifier(Modifier::BOLD)),
            value,
        ])
    };
    let cfg = &app.settings.cfg;
    f.render_widget(
        Paragraph::new(vec![
            row("Storage target", val(cfg.download_dir.clone(), p.highlight)),
            row("Active theme", val(cfg.theme.name(), p.secondary)),
            row("Threads", val(cfg.concurrent_fragments.to_string(), p.primary)),
            Line::from(""),
            Line::styled("This panel is read-only — use j/k to move around the sidebar.", Style::new().fg(p.dim)),
        ])
        .block(panel(" Environment ", p.dim)),
        env,
    );
}

fn render_downloader(f: &mut Frame, app: &App, area: Rect) {
    use Constraint::{Length, Min};

    let p = app.palette();
    let focused = app.focus == Focus::Content;
    let dl = &app.dl;

    let title = if focused {
        " YouTube Media Downloader · Esc to exit "
    } else {
        " YouTube Media Downloader · Enter to start "
    };
    let frame = panel(title, if focused { p.danger } else { p.dim });
    let inner = frame.inner(area);
    f.render_widget(frame, area);

    let heading = |lines: Vec<String>| {
        Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>()).style(Style::new().fg(p.highlight))
    };

    match dl.step {
        Step::Url => {
            let [hint, input, _] = Layout::vertical([Length(2), Length(3), Min(0)]).areas(inner);
            let text = if focused {
                "Step 1 of 3 — paste a URL, Enter to continue (Ctrl+U clears)"
            } else {
                "Press Enter to focus and paste a URL"
            };
            f.render_widget(heading(vec![text.into()]), hint);

            let width = input.width.saturating_sub(3) as usize;
            let shown = tail_chars(&dl.url, width);
            let cursor = if focused { "█" } else { "" };
            f.render_widget(
                Paragraph::new(format!("{shown}{cursor}"))
                    .style(Style::new().fg(p.text))
                    .block(panel(" Video URL ", if focused { p.primary } else { p.dim })),
                input,
            );
        }
        Step::Kind => {
            let [head, list, _] = Layout::vertical([Length(2), Length(4), Min(0)]).areas(inner);
            f.render_widget(
                heading(vec![
                    format!("URL: {}", dl.url),
                    "Step 2 of 3 — choose media type (j/k, Enter · h to go back)".into(),
                ]),
                head,
            );
            let items = Kind::ALL.iter().map(|k| format!(" {}", k.label())).collect();
            render_choices(f, list, "Media Type", items, dl.kind_idx, p.primary, &p);
        }
        Step::Format => {
            let formats = dl.formats();
            let [head, list, _] = Layout::vertical([Length(3), Length(formats.len() as u16 + 2), Min(0)]).areas(inner);
            f.render_widget(
                heading(vec![
                    format!("URL: {}", dl.url),
                    format!("Target directory: {}", app.settings.cfg.download_dir),
                    "Step 3 of 3 — choose a format, Enter to run yt-dlp".into(),
                ]),
                head,
            );
            let items = formats.iter().map(|fmt| format!(" .{fmt}")).collect();
            render_choices(f, list, "Container / Codec", items, dl.format_idx, p.success, &p);
        }
        Step::Running | Step::Done => {
            let (state, pct, lines) = {
                let job = dl.job.lock().unwrap();
                let keep = if dl.step == Step::Running { inner.height as usize } else { 8 };
                let start = job.lines.len().saturating_sub(keep);
                let lines: Vec<Line> = job.lines[start..].iter().cloned().map(Line::from).collect();
                (job.state, job.percent, lines)
            };

            let (headline, color) = match state {
                JobState::Running => ("⏳ Downloading…", p.highlight),
                JobState::Succeeded => ("✔ Download complete", p.success),
                JobState::Failed => ("✖ Download failed", p.danger),
            };

            let [head, bar, log, foot] =
                Layout::vertical([Length(2), Length(3), Min(1), Length(1)]).areas(inner);
            f.render_widget(
                Paragraph::new(headline).style(Style::new().fg(color).add_modifier(Modifier::BOLD)),
                head,
            );
            f.render_widget(gauge(" Progress ".into(), pct as u16, color, &p), bar);
            f.render_widget(
                Paragraph::new(lines).style(Style::new().fg(p.text)).wrap(Wrap { trim: true }),
                log,
            );
            let hint = if state == JobState::Running {
                "Esc: back to sidebar (download continues)"
            } else {
                "Enter / r: download another · Esc: back to sidebar"
            };
            f.render_widget(Paragraph::new(hint).style(Style::new().fg(p.dim)), foot);
        }
    }
}

fn render_analytics(f: &mut Frame, app: &App, area: Rect) {
    let p = app.palette();
    let focused = app.focus == Focus::Content;

    let ok = app.history.iter().filter(|h| h.ok).count();
    let failed = app.history.len() - ok;

    let mut lines = vec![
        Line::from(vec![
            val("✔ ", p.success),
            Span::raw(format!("{ok} completed    ")),
            val("✖ ", p.danger),
            Span::raw(format!("{failed} failed")),
        ]),
        Line::from(""),
    ];
    if app.history.is_empty() {
        lines.push(Line::styled(
            "Nothing downloaded yet — open Download Media to get started.",
            Style::new().fg(p.dim),
        ));
    }
    for h in app.history.iter().rev() {
        let (icon, color) = if h.ok { ("✔", p.success) } else { ("✖", p.danger) };
        lines.push(Line::from(vec![
            val(format!("{icon} "), color),
            val(format!(".{:<5} ", h.format), p.highlight),
            Span::raw(h.url.as_str()),
        ]));
    }

    f.render_widget(
        Paragraph::new(lines).block(panel(" Session Analytics ", if focused { p.primary } else { p.dim })),
        area,
    );
}

fn render_settings(f: &mut Frame, app: &App, area: Rect) {
    use Constraint::{Length, Min, Percentage};

    let p = app.palette();
    let focused = app.focus == Focus::Content;
    let s = &app.settings;
    let active = |field: Field| focused && s.field() == field;
    let hint = |t: &'static str| val(t, p.dim);
    let accent = |t: String| Span::styled(t, Style::new().fg(p.primary).add_modifier(Modifier::BOLD));

    let frame = panel(
        if focused { " Settings · Esc to return " } else { " Settings · Enter to configure " },
        if focused { p.secondary } else { p.dim },
    );
    let inner = frame.inner(area);
    f.render_widget(frame, area);

    let [path_a, theme_a, flags_a, save_a, footer_a] =
        Layout::vertical([Length(3), Length(3), Length(3), Length(2), Min(1)]).areas(inner);
    let [subs_a, threads_a] = Layout::horizontal([Percentage(50), Percentage(50)]).areas(flags_a);

    // Download directory
    let path_line = match &s.editing {
        Some(buf) => Line::from(vec![
            Span::styled(format!("{buf}█"), Style::new().fg(p.highlight).add_modifier(Modifier::BOLD)),
            hint("   Enter save · Esc cancel"),
        ]),
        None => Line::from(vec![val(s.cfg.download_dir.as_str(), p.text), hint("   Enter to edit")]),
    };
    f.render_widget(
        Paragraph::new(path_line).block(card("Download directory", active(Field::Path), &p)),
        path_a,
    );

    // Theme
    f.render_widget(
        Paragraph::new(Line::from(vec![
            accent("◂ ".into()),
            accent(s.cfg.theme.name().into()),
            accent(" ▸".into()),
            hint("   h/l to switch"),
        ]))
        .block(card("Colour scheme", active(Field::Theme), &p)),
        theme_a,
    );

    // Subtitles
    let subs = if s.cfg.embed_subtitles { val("● On", p.success) } else { val("○ Off", p.dim) };
    f.render_widget(
        Paragraph::new(Line::from(vec![subs, hint("   embeds EN subs (video)")]))
            .block(card("Auto subtitles", active(Field::Subtitles), &p)),
        subs_a,
    );

    // Threads
    f.render_widget(
        Paragraph::new(Line::from(vec![
            accent(format!("◂ {} ▸", s.cfg.concurrent_fragments)),
            hint("   h/l to adjust"),
        ]))
        .block(card("Download threads", active(Field::Threads), &p)),
        threads_a,
    );

    // Save button + status
    let button = if active(Field::Save) {
        Style::new().bg(p.success).fg(ON_ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::new().bg(p.surface).fg(p.text)
    };
    let status = match &s.status {
        Some((msg, ok)) => Span::styled(
            msg.as_str(),
            Style::new().fg(if *ok { p.success } else { p.danger }).add_modifier(Modifier::BOLD),
        ),
        None => hint("Enter to write config to disk"),
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(" Save settings ", button), Span::raw("  "), status])),
        save_a,
    );

    f.render_widget(
        Paragraph::new("j/k move · h/l adjust · Enter edit / toggle · Esc back").style(Style::new().fg(p.dim)),
        footer_a,
    );
}

fn render_help(f: &mut Frame, app: &App, area: Rect) {
    let p = app.palette();
    let section = |t: &'static str| Line::styled(t, Style::new().fg(p.highlight).add_modifier(Modifier::BOLD));
    let key = |k: &'static str, d: &'static str| {
        Line::from(vec![Span::styled(format!("  {k:<18}"), Style::new().fg(p.primary)), Span::raw(d)])
    };

    let lines = vec![
        section("Anywhere"),
        key("Ctrl+C", "Quit"),
        Line::from(""),
        section("Sidebar"),
        key("j/k  ↑/↓", "Move selection"),
        key("Enter  l  →", "Open the selected panel"),
        key("q  Esc", "Quit"),
        Line::from(""),
        section("Inside a panel"),
        key("j/k  ↑/↓", "Move between options"),
        key("Enter", "Confirm, advance or toggle"),
        key("h  ←  Backspace", "Go back / decrease"),
        key("l  →", "Increase / next"),
        key("Esc", "Return to the sidebar"),
        Line::from(""),
        section("Text fields"),
        key("Ctrl+U", "Clear the field"),
    ];

    f.render_widget(
        Paragraph::new(lines).style(Style::new().fg(p.text)).block(panel(" Help ", p.dim)),
        area,
    );
}
