//! thinkbot — a lightweight Linux system monitor and `yt-dlp` terminal UI.
//!
//! Architecture overview:
//! - **Host Telemetry (`StaticHostInfo`, `SystemSnapshot`):** Static system properties
//!   (OS name, kernel release, root mount type, desktop session, sysfs sensor paths)
//!   are resolved once on startup. Dynamic metrics (CPU usage via `/proc/stat` deltas,
//!   available memory via `/proc/meminfo`, load average, battery, and package temperature)
//!   are polled on a 2-second interval.
//! - **Media Downloader (`Downloader`, `Job`):** Spawns `yt-dlp` in a background worker
//!   thread while streaming stdout/stderr into a bounded `VecDeque` ring buffer. A shared
//!   `AtomicBool` cancellation flag allows immediate subprocess termination either by
//!   user request or when the application exits.
//! - **Render Loop (`run`, `ui`):** Uses a `dirty` flag and adaptive poll timeouts so
//!   Ratatui only redraws the terminal when state actually changes.

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
    collections::VecDeque,
    env, fs,
    io::{self, BufRead, BufReader, Read},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Event poll interval while a background download is actively streaming output.
const ACTIVE_TICK: Duration = Duration::from_millis(80);

/// Maximum event poll interval when the application is idle.
const IDLE_TICK: Duration = Duration::from_millis(500);

/// Interval between `/proc` and `/sys` telemetry refreshes.
const SYSTEM_REFRESH: Duration = Duration::from_secs(2);

/// Maximum number of `yt-dlp` output lines retained in memory per job.
const MAX_LOG_LINES: usize = 200;

/// Foreground text color used on top of bright accent backgrounds.
/// Uses an explicit RGB value rather than `Color::Black` so custom terminal
/// ANSI palettes do not wash out highlighted items.
const ON_ACCENT: Color = hex(0x11111b);

// =========================================================================
// General Utilities & Text Input Helpers
// =========================================================================

/// Modular arithmetic helper to cycle `idx` by `delta` within `0..len`.
fn wrap(idx: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    (idx as i32 + delta).rem_euclid(len as i32) as usize
}

/// Reads `$HOME` from the process environment.
fn home() -> Option<String> {
    env::var("HOME").ok()
}

/// Expands a leading `~` or `~/...` prefix into the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    let trimmed = path.trim();
    if trimmed == "~" {
        if let Some(h) = home() {
            return PathBuf::from(h);
        }
    }
    match (trimmed.strip_prefix("~/"), home()) {
        (Some(rest), Some(h)) => PathBuf::from(h).join(rest),
        _ => PathBuf::from(trimmed),
    }
}

/// Reads a file to a `String` and trims surrounding whitespace.
fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Checks whether an executable named `bin` exists in any directory in `$PATH`.
fn has_command(bin: &str) -> bool {
    env::var_os("PATH").is_some_and(|paths| {
        env::split_paths(&paths).any(|dir| {
            let full = dir.join(bin);
            full.is_file()
        })
    })
}

/// Returns a borrowed slice of the last `n` Unicode scalar values in `s`.
/// Used by single-line text inputs so the cursor stays visible when a URL or path
/// exceeds the width of the input box.
fn tail_chars(s: &str, n: usize) -> &str {
    let total = s.chars().count();
    let skip = total.saturating_sub(n);
    match s.char_indices().nth(skip) {
        Some((byte_idx, _)) => &s[byte_idx..],
        None => "",
    }
}

/// Maps vertical navigation keys (`j`/`Down` and `k`/`Up`) to index deltas.
fn vertical(code: KeyCode) -> Option<i32> {
    match code {
        KeyCode::Down | KeyCode::Char('j') => Some(1),
        KeyCode::Up | KeyCode::Char('k') => Some(-1),
        _ => None,
    }
}

/// Returns `true` if `code` is a backward navigation key (`h`, `Left`, or `Backspace`).
fn is_back(code: KeyCode) -> bool {
    matches!(code, KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h'))
}

/// Deletes the trailing word or URL/path segment from `buf` (`Ctrl+W` / `Ctrl+Backspace`).
/// Stops at whitespace as well as common URL/path delimiters (`/`, `&`, `?`).
fn pop_word(buf: &mut String) {
    let trimmed_len = buf.trim_end().len();
    buf.truncate(trimmed_len);
    if let Some(idx) = buf.rfind(|c: char| c.is_whitespace() || c == '/' || c == '&' || c == '?') {
        if idx + 1 == buf.len() {
            buf.pop();
        } else {
            buf.truncate(idx + 1);
        }
    } else {
        buf.clear();
    }
}

/// Processes a single key press against a mutable text buffer.
/// Supports standard character insertion, `Backspace`, `Ctrl+U` (clear line),
/// and `Ctrl+W` / `Ctrl+Backspace` (delete previous word/segment).
fn edit_text(buf: &mut String, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match (key.code, ctrl) {
        (KeyCode::Char('u'), true) => buf.clear(),
        (KeyCode::Char('w') | KeyCode::Char('h'), true) => pop_word(buf),
        (KeyCode::Char(c), false) if !c.is_control() => buf.push(c),
        (KeyCode::Backspace, true) => pop_word(buf),
        (KeyCode::Backspace, false) => {
            buf.pop();
        }
        _ => return false,
    }
    true
}

// =========================================================================
// Theme Engine & Color Palettes
// =========================================================================

/// Converts a 24-bit `0xRRGGBB` literal into a `ratatui::style::Color`.
const fn hex(c: u32) -> Color {
    Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// Semantic color slots used across all UI widgets.
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

/// Built-in color themes persisted in `config.toml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Theme {
    CatppuccinMocha,
    CatppuccinMacchiato,
    TokyoNight,
    Nord,
    GruvboxDark,
    Dracula,
    RosePine,
    Kanagawa,
    Everforest,
    OneDark,
    SolarizedDark,
    AyuMirage,
    Nightfox,
    MaterialOcean,
    HorizonDark,
    Monokai,
    Synthwave84,
    Cyberpunk,
}

impl Theme {
    /// Ordered list of all selectable themes in the Settings panel.
    const ALL: [Theme; 18] = [
        Theme::CatppuccinMocha,
        Theme::CatppuccinMacchiato,
        Theme::TokyoNight,
        Theme::Nord,
        Theme::GruvboxDark,
        Theme::Dracula,
        Theme::RosePine,
        Theme::Kanagawa,
        Theme::Everforest,
        Theme::OneDark,
        Theme::SolarizedDark,
        Theme::AyuMirage,
        Theme::Nightfox,
        Theme::MaterialOcean,
        Theme::HorizonDark,
        Theme::Monokai,
        Theme::Synthwave84,
        Theme::Cyberpunk,
    ];

    /// Human-readable display name shown in the UI.
    const fn name(self) -> &'static str {
        match self {
            Theme::CatppuccinMocha => "Catppuccin Mocha",
            Theme::CatppuccinMacchiato => "Catppuccin Macchiato",
            Theme::TokyoNight => "Tokyo Night",
            Theme::Nord => "Nord Ice",
            Theme::GruvboxDark => "Gruvbox Dark",
            Theme::Dracula => "Dracula",
            Theme::RosePine => "Rosé Pine",
            Theme::Kanagawa => "Kanagawa Wave",
            Theme::Everforest => "Everforest Dark",
            Theme::OneDark => "One Dark Pro",
            Theme::SolarizedDark => "Solarized Dark",
            Theme::AyuMirage => "Ayu Mirage",
            Theme::Nightfox => "Nightfox",
            Theme::MaterialOcean => "Material Ocean",
            Theme::HorizonDark => "Horizon Dark",
            Theme::Monokai => "Monokai Pro",
            Theme::Synthwave84 => "Synthwave '84",
            Theme::Cyberpunk => "Cyberpunk Neon",
        }
    }

    /// Steps forward or backward through `Theme::ALL`.
    fn cycle(self, delta: i32) -> Self {
        let i = Self::ALL.iter().position(|&t| t == self).unwrap_or(0);
        Self::ALL[wrap(i, Self::ALL.len(), delta)]
    }

    /// Maps a `Theme` variant to its 8 semantic colors:
    /// `[primary, secondary, highlight, text, dim, surface, success, danger]`.
    const fn palette(self) -> Palette {
        let c = match self {
            Theme::CatppuccinMocha => [
                0x89b4fa, 0xcba6f7, 0xf9e2af, 0xcdd6f4, 0x6c7086, 0x313244, 0xa6e3a1, 0xf38ba8,
            ],
            Theme::CatppuccinMacchiato => [
                0x8aadf4, 0xc6a0f6, 0xeed49f, 0xcad3f5, 0x6e738d, 0x363a4f, 0xa6da95, 0xed8796,
            ],
            Theme::TokyoNight => [
                0x7aa2f7, 0xbb9af7, 0xe0af68, 0xc0caf5, 0x565f89, 0x24283b, 0x9ece6a, 0xf7768e,
            ],
            Theme::Nord => [
                0x88c0d0, 0xb48ead, 0xebcb8b, 0xeceff4, 0x616e88, 0x3b4252, 0xa3be8c, 0xbf616a,
            ],
            Theme::GruvboxDark => [
                0xfe8019, 0xd3869b, 0xfabd2f, 0xebdbb2, 0x7c6f64, 0x3c3836, 0xb8bb26, 0xfb4934,
            ],
            Theme::Dracula => [
                0xbd93f9, 0xff79c6, 0xf1fa8c, 0xf8f8f2, 0x6272a4, 0x44475a, 0x50fa7b, 0xff5555,
            ],
            Theme::RosePine => [
                0xebbcba, 0xc4a7e7, 0xf6c177, 0xe0def4, 0x6e6a86, 0x26233a, 0x9ccfd8, 0xeb6f92,
            ],
            Theme::Kanagawa => [
                0x7e9cd8, 0x957fb8, 0xe6c384, 0xdcd7ba, 0x727169, 0x2a2a37, 0x98bb6c, 0xe46876,
            ],
            Theme::Everforest => [
                0x7fbbb3, 0xd699b6, 0xdbbc7f, 0xd3c6aa, 0x859289, 0x343f44, 0xa7c080, 0xe67e80,
            ],
            Theme::OneDark => [
                0x61afef, 0xc678dd, 0xe5c07b, 0xabb2bf, 0x5c6370, 0x2c323c, 0x98c379, 0xe06c75,
            ],
            Theme::SolarizedDark => [
                0x268bd2, 0x6c71c4, 0xb58900, 0x93a1a1, 0x586e75, 0x073642, 0x859900, 0xdc322f,
            ],
            Theme::AyuMirage => [
                0x73d0ff, 0xd4bfff, 0xffcc66, 0xcccac2, 0x5c6773, 0x242936, 0xbae67e, 0xf28779,
            ],
            Theme::Nightfox => [
                0x719cd6, 0x9d79d6, 0xdbc074, 0xcdcecf, 0x738091, 0x29394f, 0x81b29a, 0xc94f6d,
            ],
            Theme::MaterialOcean => [
                0x82aaff, 0xc792ea, 0xffcb6b, 0xeeffff, 0x546e7a, 0x263238, 0xc3e88d, 0xf07178,
            ],
            Theme::HorizonDark => [
                0xe95678, 0xb877db, 0xfab795, 0xd5d8da, 0x6c6f93, 0x2e303e, 0x27d797, 0xf43e5c,
            ],
            Theme::Monokai => [
                0x66d9ef, 0xab9df2, 0xe6db74, 0xf8f8f2, 0x75715e, 0x3e3d32, 0xa6e22e, 0xff6188,
            ],
            Theme::Synthwave84 => [
                0x36f9f6, 0xff7edb, 0xfede5d, 0xffffff, 0x848bbd, 0x2a2139, 0x72f1b8, 0xfe4450,
            ],
            Theme::Cyberpunk => [
                0x00ffea, 0xff007f, 0xfee715, 0xffffff, 0x5c5c78, 0x1a1a2e, 0x39ff14, 0xff3131,
            ],
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

// =========================================================================
// Configuration Persistence
// =========================================================================

/// User configuration serialized to `$XDG_CONFIG_HOME/thinkbot/config.toml`.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
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
    /// Resolves the configuration file path (`$XDG_CONFIG_HOME/thinkbot/config.toml`
    /// or `~/.config/thinkbot/config.toml`).
    fn path() -> PathBuf {
        let base = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(home().unwrap_or_else(|| ".".into())).join(".config"));
        base.join("thinkbot").join("config.toml")
    }

    /// Loads `config.toml` from disk, falling back to `Config::default()` if missing
    /// or malformed. Clamps `concurrent_fragments` to `1..=16`.
    fn load() -> Self {
        let mut cfg: Self = read_trimmed(Self::path())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        cfg.concurrent_fragments = cfg.concurrent_fragments.clamp(1, 16);
        cfg
    }

    /// Writes the current configuration to disk, creating parent directories if needed.
    fn save(&self) -> io::Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self).map_err(io::Error::other)?;
        fs::write(path, text)
    }
}

// =========================================================================
// Sidebar Navigation & Focus State
// =========================================================================

/// Top-level panels accessible from the left navigation sidebar.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Overview,
    Download,
    Analytics,
    Settings,
    Help,
    Update,
}

impl Menu {
    const ALL: [Menu; 6] = [
        Menu::Overview,
        Menu::Download,
        Menu::Analytics,
        Menu::Settings,
        Menu::Help,
        Menu::Update,
    ];

    const fn label(self) -> &'static str {
        match self {
            Menu::Overview => "◈ System Overview",
            Menu::Download => "▼ Download Media",
            Menu::Analytics => "▲ Analytics",
            Menu::Settings => "◆ Settings",
            Menu::Help => "? Help",
            Menu::Update => "↻ Update & Reboot",
        }
    }

    /// Returns `true` if pressing `Enter` moves keyboard focus into the right-hand panel.
    /// `Overview` and `Help` are read-only views, and `Update` triggers an immediate action.
    const fn is_enterable(self) -> bool {
        !matches!(self, Menu::Overview | Menu::Help | Menu::Update)
    }
}

/// Tracks which pane currently receives directional and action keys.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sidebar,
    Content,
}

// =========================================================================
// Media Downloader & Subprocess Management
// =========================================================================

/// Target media stream type for `yt-dlp`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Video,
    Audio,
}

impl Kind {
    const ALL: [Kind; 2] = [Kind::Video, Kind::Audio];

    const fn label(self) -> &'static str {
        match self {
            Kind::Video => "▤ Video",
            Kind::Audio => "♪ Audio",
        }
    }

    /// Supported output container/codec extensions for each media type.
    const fn formats(self) -> &'static [&'static str] {
        match self {
            Kind::Video => &["mp4", "mkv", "webm", "mov"],
            Kind::Audio => &["mp3", "m4a", "flac", "wav", "opus"],
        }
    }
}

/// Step state machine for the downloader wizard.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Url,
    Kind,
    Format,
    Running,
    Done,
}

/// Lifecycle state of a background `yt-dlp` process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum JobState {
    #[default]
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

/// Shared state between a background `yt-dlp` worker thread and the UI thread.
struct Job {
    /// Bounded ring buffer of trimmed stdout/stderr lines.
    lines: VecDeque<String>,
    /// Latest parsed download percentage (`0.0..=100.0`).
    percent: f64,
    /// Latest parsed transfer speed (e.g., `"5.20MiB/s"`).
    speed: Option<String>,
    /// Latest parsed estimated time remaining (e.g., `"00:12"`).
    eta: Option<String>,
    /// Execution status of the child process.
    state: JobState,
    /// Monotonically increasing counter incremented on every state or log update.
    /// Allows the UI thread to skip redraws when nothing changed.
    version: u64,
}

impl Default for Job {
    fn default() -> Self {
        Self {
            lines: VecDeque::with_capacity(MAX_LOG_LINES),
            percent: 0.0,
            speed: None,
            eta: None,
            state: JobState::Running,
            version: 0,
        }
    }
}

/// Identifies `yt-dlp` progress lines such as:
/// `[download]  45.0% of   50.00MiB at    6.10MiB/s ETA 00:05`
fn is_progress(line: &str) -> bool {
    line.starts_with("[download]") && line.contains("% of")
}

impl Job {
    /// Appends a line of output from `yt-dlp`, parsing progress telemetry and
    /// collapsing consecutive `[download] ... % of ...` updates in-place.
    fn push(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if is_progress(line) {
            if let Some(pct) = line
                .split('%')
                .next()
                .and_then(|s| s.rsplit(char::is_whitespace).next())
            {
                self.percent = pct.parse().unwrap_or(self.percent);
            }
            if let Some(after_at) = line.split(" at ").nth(1) {
                let mut parts = after_at.split_whitespace();
                if let Some(spd) = parts.next() {
                    if spd != "Unknown" {
                        self.speed = Some(spd.to_string());
                    }
                }
            }
            if let Some(after_eta) = line.split("ETA ").nth(1) {
                if let Some(eta) = after_eta.split_whitespace().next() {
                    if eta != "Unknown" {
                        self.eta = Some(eta.to_string());
                    }
                }
            }
            // Replace the previous progress line instead of flooding the log view.
            if self.lines.back().is_some_and(|l| is_progress(l)) {
                self.lines.pop_back();
            }
        }
        if self.lines.len() >= MAX_LOG_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line.to_string());
        self.version = self.version.wrapping_add(1);
    }
}

/// Immutable parameters passed to `run_download` when starting a job.
#[derive(Clone)]
struct Request {
    url: String,
    kind: Kind,
    format: &'static str,
    dir: PathBuf,
    subtitles: bool,
    threads: u8,
}

/// Spawns a reader thread that drains a child pipe (`stdout` or `stderr`) line-by-line
/// into the shared `Job` buffer.
fn pump(reader: impl Read + Send + 'static, job: Arc<Mutex<Job>>) -> JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if let Ok(mut j) = job.lock() {
                j.push(&line);
            }
        }
    })
}

/// Launches `run_download` on a background thread and returns the shared `Job` state
/// and cancellation flag.
fn spawn_download(req: Request) -> (Arc<Mutex<Job>>, Arc<AtomicBool>) {
    let job = Arc::new(Mutex::new(Job::default()));
    let cancel = Arc::new(AtomicBool::new(false));
    let shared_job = Arc::clone(&job);
    let shared_cancel = Arc::clone(&cancel);
    thread::spawn(move || run_download(req, shared_job, shared_cancel));
    (job, cancel)
}

/// Executes `yt-dlp` as a child process and polls for completion or cancellation.
fn run_download(req: Request, job: Arc<Mutex<Job>>, cancel: Arc<AtomicBool>) {
    if let Err(e) = fs::create_dir_all(&req.dir) {
        if let Ok(mut j) = job.lock() {
            j.push(&format!(
                "Failed to create output directory {}: {e}",
                req.dir.display()
            ));
            j.state = JobState::Failed;
        }
        return;
    }

    let mut cmd = Command::new("yt-dlp");
    cmd.args(["--newline", "-N"])
        .arg(req.threads.to_string())
        .arg("-P")
        .arg(&req.dir);

    match req.kind {
        Kind::Audio => cmd.args(["-x", "--audio-format", req.format]),
        Kind::Video => {
            if req.subtitles {
                cmd.args(["--write-auto-subs", "--sub-langs", "en", "--embed-subs"]);
            }
            cmd.args(["--merge-output-format", req.format])
        }
    };

    // `--` prevents a leading hyphen in the URL string from being parsed as a CLI flag.
    cmd.arg("--")
        .arg(&req.url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Ok(mut j) = job.lock() {
        j.push(&format!("Saving to {}", req.dir.display()));
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            if let Ok(mut j) = job.lock() {
                j.push(&format!("Failed to launch yt-dlp: {e}"));
                j.push("Make sure yt-dlp is installed and on your PATH.");
                j.state = JobState::Failed;
            }
            return;
        }
    };

    let readers = [
        child.stdout.take().map(|o| pump(o, Arc::clone(&job))),
        child.stderr.take().map(|e| pump(e, Arc::clone(&job))),
    ];

    // Poll `try_wait` every 50ms so we can respond promptly to the cancellation flag
    // without blocking indefinitely on `child.wait()`.
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            for handle in readers.into_iter().flatten() {
                let _ = handle.join();
            }
            if let Ok(mut j) = job.lock() {
                j.push("Download cancelled by user.");
                j.state = JobState::Cancelled;
            }
            return;
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(e) => break Err(e),
        }
    };

    for handle in readers.into_iter().flatten() {
        let _ = handle.join();
    }

    if let Ok(mut j) = job.lock() {
        match status {
            Ok(s) if s.success() => {
                j.percent = 100.0;
                j.eta = None;
                j.state = JobState::Succeeded;
                j.version = j.version.wrapping_add(1);
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
}

/// UI state for the media downloader wizard.
struct Downloader {
    step: Step,
    url: String,
    kind_idx: usize,
    format_idx: usize,
    job: Arc<Mutex<Job>>,
    cancel: Arc<AtomicBool>,
    last_seen_version: u64,
    started_at: Option<Instant>,
}

impl Downloader {
    fn new() -> Self {
        Self {
            step: Step::Url,
            url: String::new(),
            kind_idx: 0,
            format_idx: 0,
            job: Arc::default(),
            cancel: Arc::new(AtomicBool::new(false)),
            last_seen_version: 0,
            started_at: None,
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

    /// Signals the background worker thread to terminate the running `yt-dlp` process.
    fn cancel_running(&self) {
        if self.step == Step::Running {
            self.cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// Completed, failed, or cancelled job record displayed in the Analytics panel.
struct HistoryEntry {
    url: String,
    kind: Kind,
    format: &'static str,
    state: JobState,
    elapsed_secs: u64,
}

// =========================================================================
// Settings Panel State
// =========================================================================

/// Interactive fields within the Settings panel.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Path,
    Theme,
    Subtitles,
    Threads,
    Save,
}

impl Field {
    const ALL: [Field; 5] = [
        Field::Path,
        Field::Theme,
        Field::Subtitles,
        Field::Threads,
        Field::Save,
    ];
}

/// Manages in-memory configuration edits and tracks whether changes have been written to disk.
struct Settings {
    cfg: Config,
    saved_cfg: Config,
    field: usize,
    /// Holds the temporary edit buffer while the user is typing a new download directory path.
    editing: Option<String>,
    /// Last save result message and success flag (`true` = saved, `false` = I/O error).
    status: Option<(String, bool)>,
}

impl Settings {
    fn new() -> Self {
        let cfg = Config::load();
        Self {
            saved_cfg: cfg.clone(),
            cfg,
            field: 0,
            editing: None,
            status: None,
        }
    }

    /// Returns `true` if the active in-memory config differs from the last saved state on disk.
    fn is_dirty(&self) -> bool {
        self.cfg != self.saved_cfg
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
        match self.cfg.save() {
            Ok(()) => {
                self.saved_cfg = self.cfg.clone();
                self.status = Some((format!("✔ Saved to {}", Config::path().display()), true));
            }
            Err(e) => {
                self.status = Some((format!("✖ Could not save config: {e}"), false));
            }
        }
    }
}

// =========================================================================
// Host Telemetry (Static Detection + Dynamic Polling)
// =========================================================================

/// Host properties and sysfs sensor paths resolved once at startup.
struct StaticHostInfo {
    os: String,
    host: String,
    kernel: String,
    root_fs: String,
    desktop: String,
    battery_dir: Option<PathBuf>,
    thermal_zone: Option<PathBuf>,
    has_ytdlp: bool,
    has_ffmpeg: bool,
}

impl StaticHostInfo {
    fn detect() -> Self {
        // Parse `PRETTY_NAME` from `/etc/os-release`.
        let os_name = read_trimmed("/etc/os-release")
            .and_then(|s| {
                s.lines().find_map(|l| {
                    l.strip_prefix("PRETTY_NAME=")
                        .map(|v| v.trim_matches('"').to_string())
                })
            })
            .unwrap_or_else(|| "Linux".into());

        // Find the filesystem type for the `/` mount in `/proc/mounts`.
        let root_fs = read_trimmed("/proc/mounts")
            .and_then(|s| {
                s.lines()
                    .filter_map(|l| {
                        let mut f = l.split_whitespace().skip(1);
                        (f.next()? == "/").then(|| f.next().unwrap_or("?").to_string())
                    })
                    .next_back()
            })
            .unwrap_or_else(|| "unknown".into());

        let desktop = match (env::var("XDG_CURRENT_DESKTOP"), env::var("XDG_SESSION_TYPE")) {
            (Ok(d), Ok(s)) if !d.is_empty() => format!("{d} ({s})"),
            (Ok(d), _) if !d.is_empty() => d,
            _ => "unknown".into(),
        };

        // Locate the first `BAT*` entry under `/sys/class/power_supply`.
        let battery_dir = fs::read_dir("/sys/class/power_supply")
            .ok()
            .and_then(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("BAT")))
            });

        // Prefer the `x86_pkg_temp` thermal zone if available; otherwise use the first zone.
        let thermal_zone = fs::read_dir("/sys/class/thermal").ok().and_then(|entries| {
            let mut zones: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("thermal_zone"))
                })
                .collect();
            zones.sort();
            zones
                .iter()
                .find(|z| read_trimmed(z.join("type")).as_deref() == Some("x86_pkg_temp"))
                .or_else(|| zones.first())
                .cloned()
        });

        Self {
            os: format!("{os_name} {}", env::consts::ARCH),
            host: read_trimmed("/proc/sys/kernel/hostname").unwrap_or_else(|| "unknown".into()),
            kernel: read_trimmed("/proc/sys/kernel/osrelease").unwrap_or_else(|| "unknown".into()),
            root_fs,
            desktop,
            battery_dir,
            thermal_zone,
            has_ytdlp: has_command("yt-dlp"),
            has_ffmpeg: has_command("ffmpeg"),
        }
    }

    /// Re-checks whether `yt-dlp` and `ffmpeg` are present in `$PATH`.
    fn refresh_tools(&mut self) {
        self.has_ytdlp = has_command("yt-dlp");
        self.has_ffmpeg = has_command("ffmpeg");
    }
}

/// Cumulative CPU jiffies read from the first `cpu` line of `/proc/stat`.
#[derive(Clone, Copy, Default)]
struct CpuSample {
    total: u64,
    idle: u64,
}

/// Reads aggregate CPU time counters from `/proc/stat`.
/// Fields are: user, nice, system, idle, iowait, irq, softirq, steal.
fn read_cpu_sample() -> Option<CpuSample> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let mut parts = line.split_whitespace();
    if parts.next()? != "cpu" {
        return None;
    }
    let vals: Vec<u64> = parts.filter_map(|p| p.parse().ok()).collect();
    if vals.len() < 4 {
        return None;
    }
    let total: u64 = vals.iter().sum();
    let idle = vals[3] + vals.get(4).copied().unwrap_or(0);
    Some(CpuSample { total, idle })
}

/// Dynamic host telemetry refreshed every `SYSTEM_REFRESH` (2 seconds).
struct SystemSnapshot {
    uptime: String,
    load: String,
    cpu_pct: u16,
    prev_cpu: Option<CpuSample>,
    mem_used_mib: u64,
    mem_total_mib: u64,
    battery: Option<(u16, String)>,
    temp_c: Option<u16>,
}

/// Formats uptime seconds into `Xd Xh Xm` or `Xh Xm`.
fn format_uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else {
        format!("{h}h {m}m")
    }
}

impl SystemSnapshot {
    /// Polls `/proc/uptime`, `/proc/meminfo`, `/proc/stat`, `/proc/loadavg`,
    /// and the cached battery/thermal sensor paths.
    fn poll(host: &StaticHostInfo, prev: Option<CpuSample>) -> Self {
        let uptime_secs = read_trimmed("/proc/uptime")
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
            .unwrap_or(0.0) as u64;

        let meminfo = read_trimmed("/proc/meminfo").unwrap_or_default();
        let mut total = 0u64;
        let mut avail = 0u64;
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                total = rest
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
                avail = rest
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
            }
            if total > 0 && avail > 0 {
                break;
            }
        }

        // Compute CPU utilization percentage over the interval since `prev`.
        let curr_cpu = read_cpu_sample();
        let cpu_pct = match (prev, curr_cpu) {
            (Some(p), Some(c)) => {
                let d_total = c.total.saturating_sub(p.total);
                let d_idle = c.idle.saturating_sub(p.idle);
                let d_used = d_total.saturating_sub(d_idle);
                (d_used * 100).checked_div(d_total).unwrap_or(0).min(100) as u16
            }
            _ => 0,
        };

        let battery = host.battery_dir.as_ref().and_then(|dir| {
            let pct = read_trimmed(dir.join("capacity"))?.parse().ok()?;
            let status = read_trimmed(dir.join("status")).unwrap_or_else(|| "Unknown".into());
            Some((pct, status))
        });

        let temp_c = host.thermal_zone.as_ref().and_then(|zone| {
            let millideg: u32 = read_trimmed(zone.join("temp"))?.parse().ok()?;
            Some((millideg / 1000) as u16)
        });

        Self {
            uptime: format_uptime(uptime_secs),
            load: read_trimmed("/proc/loadavg")
                .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
                .unwrap_or_else(|| "n/a".into()),
            cpu_pct,
            prev_cpu: curr_cpu,
            mem_used_mib: total.saturating_sub(avail) / 1024,
            mem_total_mib: total / 1024,
            battery,
            temp_c,
        }
    }
}

// =========================================================================
// Application Root State & Lifecycle
// =========================================================================

struct App {
    menu: usize,
    focus: Focus,
    quit: bool,
    rebuild: bool,
    dirty: bool,
    dl: Downloader,
    settings: Settings,
    host: StaticHostInfo,
    sys: SystemSnapshot,
    last_poll: Instant,
    history: Vec<HistoryEntry>,
    history_idx: usize,
}

impl App {
    fn new() -> Self {
        let host = StaticHostInfo::detect();
        let first_cpu = read_cpu_sample();
        let sys = SystemSnapshot::poll(&host, first_cpu);
        Self {
            menu: 0,
            focus: Focus::Sidebar,
            quit: false,
            rebuild: false,
            dirty: true,
            dl: Downloader::new(),
            settings: Settings::new(),
            host,
            sys,
            last_poll: Instant::now(),
            history: Vec::new(),
            history_idx: 0,
        }
    }

    /// Saves any pending configuration edits and signals the main loop to exit
    /// into `rebuild_and_restart()`.
    fn trigger_rebuild(&mut self) {
        if self.settings.is_dirty() {
            let _ = self.settings.cfg.save();
        }
        self.rebuild = true;
        self.quit = true;
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
        let (job, cancel) = spawn_download(req);
        self.dl.job = job;
        self.dl.cancel = cancel;
        self.dl.last_seen_version = 0;
        self.dl.started_at = Some(Instant::now());
        self.dl.step = Step::Running;
        self.dirty = true;
    }

    fn reset_download(&mut self) {
        self.dl = Downloader::new();
        self.dirty = true;
    }

    fn refresh_system(&mut self) {
        self.host.refresh_tools();
        self.sys = SystemSnapshot::poll(&self.host, self.sys.prev_cpu);
        self.last_poll = Instant::now();
        self.dirty = true;
    }

    /// Periodic maintenance: refreshes `/proc` and `/sys` metrics when `SYSTEM_REFRESH`
    /// elapses and transitions finished background downloads into `Step::Done`.
    fn tick(&mut self) {
        if self.last_poll.elapsed() >= SYSTEM_REFRESH {
            self.sys = SystemSnapshot::poll(&self.host, self.sys.prev_cpu);
            self.last_poll = Instant::now();
            if self.selected() == Menu::Overview {
                self.dirty = true;
            }
        }

        if self.dl.step == Step::Running {
            let (state, version) = {
                let job = self.dl.job.lock().unwrap();
                (job.state, job.version)
            };
            if version != self.dl.last_seen_version {
                self.dl.last_seen_version = version;
                self.dirty = true;
            }
            if state != JobState::Running {
                let elapsed_secs = self.dl.started_at.map_or(0, |t| t.elapsed().as_secs());
                self.history.push(HistoryEntry {
                    url: self.dl.url.trim().to_string(),
                    kind: self.dl.kind(),
                    format: self.dl.format(),
                    state,
                    elapsed_secs,
                });
                self.history_idx = 0;
                self.dl.step = Step::Done;
                self.dirty = true;
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Ensure any running `yt-dlp` subprocess is terminated when `App` is dropped.
        self.dl.cancel_running();
    }
}

/// Resolves the source repository path for in-app rebuilds.
fn project_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if manifest_dir.join("Cargo.toml").is_file() {
        manifest_dir
    } else {
        expand_tilde("~/.config/thinkbot")
    }
}

/// Pulls latest git changes (if clean), runs `cargo build --release`, atomically updates
/// `~/.local/bin/thinkbot`, and replaces the current process via `exec()`.
fn rebuild_and_restart() -> io::Result<()> {
    let dir = project_dir();
    let manifest = dir.join("Cargo.toml");
    println!(
        "\x1b[1;36m↻ Updating & rebuilding thinkbot in {}...\x1b[0m",
        dir.display()
    );

    if dir.join(".git").exists() {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["pull", "--ff-only"])
            .status();
    }

    let build_status = Command::new("cargo")
        .args(["build", "--release", "--manifest-path"])
        .arg(&manifest)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    let built_bin = dir.join("target/release/thinkbot");

    match build_status {
        Ok(s) if s.success() && built_bin.is_file() => {
            // Copy to a temporary file and rename over `~/.local/bin/thinkbot` so Linux
            // never returns ETXTBSY ("Text file busy") when updating a running binary.
            if let Some(h) = home() {
                let local_bin_dir = PathBuf::from(h).join(".local/bin");
                if local_bin_dir.is_dir() {
                    let tmp_bin = local_bin_dir.join(".thinkbot.rebuild.tmp");
                    let dest_bin = local_bin_dir.join("thinkbot");
                    if fs::copy(&built_bin, &tmp_bin).is_ok() {
                        let _ = fs::set_permissions(&tmp_bin, fs::Permissions::from_mode(0o755));
                        let _ = fs::rename(&tmp_bin, &dest_bin);
                    }
                }
            }
            println!("\x1b[1;32m✔ Build complete — restarting thinkbot...\x1b[0m");
            let err = Command::new(&built_bin).args(env::args_os().skip(1)).exec();
            Err(err)
        }
        _ => {
            eprintln!(
                "\n\x1b[1;31m✖ Rebuild failed.\x1b[0m Press Enter to relaunch previous build, or Ctrl+C to exit..."
            );
            let mut line = String::new();
            let _ = io::stdin().read_line(&mut line);
            if let Ok(exe) = env::current_exe() {
                let err = Command::new(exe).args(env::args_os().skip(1)).exec();
                return Err(err);
            }
            Ok(())
        }
    }
}

fn main() -> io::Result<()> {
    // `ratatui::init` enters the alternate screen, enables raw mode, and installs a panic
    // hook that restores the terminal before printing panic backtraces.
    let mut terminal = ratatui::init();
    let (result, rebuild) = run(&mut terminal);
    ratatui::restore();
    result?;
    if rebuild {
        rebuild_and_restart()?;
    }
    Ok(())
}

/// Main event loop. Redraws only when `app.dirty` is set and coalesces bursts of
/// input events (such as bracketed pastes) into a single frame render.
fn run(terminal: &mut DefaultTerminal) -> (io::Result<()>, bool) {
    let mut app = App::new();
    while !app.quit {
        if app.dirty {
            if let Err(e) = terminal.draw(|f| ui(f, &app)) {
                return (Err(e), false);
            }
            app.dirty = false;
        }

        let timeout = if app.dl.step == Step::Running {
            ACTIVE_TICK
        } else {
            IDLE_TICK.min(SYSTEM_REFRESH.saturating_sub(app.last_poll.elapsed()))
        };

        match event::poll(timeout) {
            Ok(true) => {
                // Drain all queued terminal events before rendering the next frame.
                loop {
                    match event::read() {
                        Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                            on_key(&mut app, key);
                            app.dirty = true;
                        }
                        Ok(Event::Paste(text)) => {
                            on_paste(&mut app, &text);
                            app.dirty = true;
                        }
                        Ok(Event::Resize(_, _)) => {
                            app.dirty = true;
                        }
                        Ok(_) => {}
                        Err(e) => return (Err(e), false),
                    }
                    match event::poll(Duration::ZERO) {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(e) => return (Err(e), false),
                    }
                }
            }
            Ok(false) => {}
            Err(e) => return (Err(e), false),
        }
        app.tick();
    }
    app.dl.cancel_running();
    (Ok(()), app.rebuild)
}

// =========================================================================
// Keyboard & Paste Input Dispatch
// =========================================================================

/// Appends pasted text (stripped of control characters/newlines) into the active text input.
fn on_paste(app: &mut App, text: &str) {
    let clean: String = text.chars().filter(|&c| !c.is_control()).collect();
    if clean.is_empty() {
        return;
    }
    match (app.focus, app.selected()) {
        (Focus::Content, Menu::Download) if app.dl.step == Step::Url => {
            app.dl.url.push_str(&clean);
        }
        (Focus::Content, Menu::Settings) => {
            if let Some(buf) = app.settings.editing.as_mut() {
                buf.push_str(&clean);
            }
        }
        _ => {}
    }
}

/// Top-level keyboard router. `Ctrl+C` always exits immediately; all other keys are
/// routed based on whether focus is in the sidebar or the active content panel.
fn on_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.quit = true;
        return;
    }
    match (app.focus, app.selected()) {
        (Focus::Sidebar, _) => sidebar_key(app, key.code),
        (Focus::Content, Menu::Download) => download_key(app, key),
        (Focus::Content, Menu::Analytics) => analytics_key(app, key.code),
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
        KeyCode::Char('r') if app.selected() == Menu::Overview => app.refresh_system(),
        KeyCode::Char('U') => app.trigger_rebuild(),
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
            if app.selected() == Menu::Update {
                app.trigger_rebuild();
            } else if app.selected().is_enterable() {
                app.focus = Focus::Content;
            }
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
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
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
        Step::Running => match code {
            KeyCode::Esc => app.focus = Focus::Sidebar,
            KeyCode::Char('c') | KeyCode::Char('x') => app.dl.cancel_running(),
            _ => {}
        },
        Step::Done => match code {
            KeyCode::Enter | KeyCode::Char('n') => app.reset_download(),
            KeyCode::Char('r') => app.start_download(),
            c if c == KeyCode::Esc || is_back(c) => app.focus = Focus::Sidebar,
            _ => {}
        },
    }
}

fn analytics_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => app.focus = Focus::Sidebar,
        c if is_back(c) => app.focus = Focus::Sidebar,
        KeyCode::Char('c') | KeyCode::Char('x') => {
            app.history.clear();
            app.history_idx = 0;
        }
        KeyCode::Enter | KeyCode::Char('r') if !app.history.is_empty() => {
            // History entries are rendered newest-first; convert the visual row index
            // back to the underlying `Vec` index.
            let rev_idx = app.history.len().saturating_sub(1 + app.history_idx);
            if let Some(entry) = app.history.get(rev_idx) {
                app.dl.url = entry.url.clone();
                app.dl.kind_idx = Kind::ALL.iter().position(|&k| k == entry.kind).unwrap_or(0);
                app.dl.format_idx = entry
                    .kind
                    .formats()
                    .iter()
                    .position(|&f| f == entry.format)
                    .unwrap_or(0);
                app.dl.step = Step::Format;
                app.menu = Menu::ALL
                    .iter()
                    .position(|&m| m == Menu::Download)
                    .unwrap_or(1);
                app.focus = Focus::Content;
            }
        }
        c => {
            if let Some(d) = vertical(c) {
                app.history_idx = wrap(app.history_idx, app.history.len(), d);
            }
        }
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

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
        s.save();
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

// =========================================================================
// Reusable UI Widget Builders
// =========================================================================

/// Creates a rounded bordered `Block` with a styled title and border color.
fn panel<'a>(title: impl Into<Line<'a>>, color: Color) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title.into())
        .border_style(Style::new().fg(color))
}

/// Creates a Settings field card, highlighting the border and prefixing `▶` when active.
fn card(title: &str, active: bool, p: &Palette) -> Block<'static> {
    if active {
        panel(format!(" ▶ {title} "), p.highlight)
    } else {
        panel(format!(" {title} "), p.dim)
    }
}

/// Shorthand for creating a colored `Span`.
fn val<'a>(text: impl Into<Cow<'a, str>>, color: Color) -> Span<'a> {
    Span::styled(text, Style::new().fg(color))
}

/// Formats a bold fixed-width key label followed by a styled value `Span`.
fn kv<'a>(label: &str, value: Span<'a>, p: &Palette) -> Line<'a> {
    let key = Span::styled(
        format!(" {label:<9}"),
        Style::new().fg(p.primary).add_modifier(Modifier::BOLD),
    );
    Line::from(vec![key, value])
}

/// Builds a Unicode progress `Gauge` bounded to `0..=100`.
fn gauge(title: String, pct: u16, color: Color, p: &Palette) -> Gauge<'static> {
    Gauge::default()
        .block(panel(title, p.dim))
        .gauge_style(Style::new().fg(color).bg(p.surface))
        .percent(pct.min(100))
        .use_unicode(true)
}

/// Renders a selectable `List` for wizard steps (media type and output format).
fn render_choices(
    f: &mut Frame,
    area: Rect,
    title: &str,
    items: Vec<String>,
    selected: usize,
    accent: Color,
    p: &Palette,
) {
    let list = List::new(items)
        .block(panel(format!(" {title} "), p.dim))
        .highlight_style(
            Style::new()
                .bg(accent)
                .fg(ON_ACCENT)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");
    f.render_stateful_widget(
        list,
        area,
        &mut ListState::default().with_selected(Some(selected)),
    );
}

// =========================================================================
// Panel Rendering Functions
// =========================================================================

fn ui(f: &mut Frame, app: &App) {
    let p = app.palette();
    let [side, main] =
        Layout::horizontal([Constraint::Length(28), Constraint::Min(40)]).areas(f.area());
    render_sidebar(f, app, &p, side);
    match app.selected() {
        Menu::Overview => render_overview(f, app, &p, main),
        Menu::Download => render_downloader(f, app, &p, main),
        Menu::Analytics => render_analytics(f, app, &p, main),
        Menu::Settings => render_settings(f, app, &p, main),
        Menu::Help => render_help(f, app, &p, main),
        Menu::Update => render_update(f, app, &p, main),
    }
}

fn render_sidebar(f: &mut Frame, app: &App, p: &Palette, area: Rect) {
    let focused = app.focus == Focus::Sidebar;

    let highlight = if focused {
        Style::new()
            .bg(p.primary)
            .fg(ON_ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().bg(p.surface).fg(p.text)
    };

    // Decorate sidebar labels with live download progress, history count, or unsaved indicators.
    let items: Vec<String> = Menu::ALL
        .iter()
        .map(|&m| match m {
            Menu::Download if app.dl.step == Step::Running => {
                let pct = app.dl.job.lock().map_or(0, |j| j.percent as u16);
                format!("{} [{pct}%]", m.label())
            }
            Menu::Analytics if !app.history.is_empty() => {
                format!("{} ({})", m.label(), app.history.len())
            }
            Menu::Settings if app.settings.is_dirty() => {
                format!("{} *", m.label())
            }
            _ => m.label().to_string(),
        })
        .collect();

    let list = List::new(items)
        .style(Style::new().fg(p.text))
        .block(
            panel(" Navigation ", if focused { p.primary } else { p.dim })
                .title_bottom(Line::styled(" q quit ", Style::new().fg(p.dim))),
        )
        .highlight_style(highlight)
        .highlight_symbol("▸ ");

    f.render_stateful_widget(
        list,
        area,
        &mut ListState::default().with_selected(Some(app.menu)),
    );
}

fn render_overview(f: &mut Frame, app: &App, p: &Palette, area: Rect) {
    let sys = &app.sys;
    let host = &app.host;

    let frame = panel(" Host Environment · r to refresh ", p.primary);
    let inner = frame.inner(area);
    f.render_widget(frame, area);

    let [ident, gauges, env] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Length(3),
        Constraint::Min(5),
    ])
    .areas(inner);

    // Two-column system identity card
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
            kv("OS", val(host.os.as_str(), p.text), p),
            kv("Host", val(host.host.as_str(), p.text), p),
            kv("Kernel", val(host.kernel.as_str(), p.text), p),
            kv("Uptime", val(sys.uptime.as_str(), p.success), p),
        ]),
        left,
    );
    f.render_widget(
        Paragraph::new(vec![
            kv("Desktop", val(host.desktop.as_str(), p.text), p),
            kv("Root FS", val(host.root_fs.as_str(), p.text), p),
            kv("Load", val(sys.load.as_str(), p.highlight), p),
            kv("Temp", temp, p),
        ]),
        right,
    );

    // Hardware resource gauges: CPU, Memory, Power
    let [cpu_area, mem_area, bat_area] = Layout::horizontal([
        Constraint::Percentage(33),
        Constraint::Percentage(34),
        Constraint::Percentage(33),
    ])
    .areas(gauges);

    let cpu_color = if sys.cpu_pct > 85 {
        p.danger
    } else if sys.cpu_pct > 60 {
        p.highlight
    } else {
        p.secondary
    };
    f.render_widget(
        gauge(format!(" CPU · {}% ", sys.cpu_pct), sys.cpu_pct, cpu_color, p),
        cpu_area,
    );

    let mem_pct = (sys.mem_used_mib * 100)
        .checked_div(sys.mem_total_mib)
        .unwrap_or(0) as u16;
    let mem_color = if mem_pct > 85 { p.danger } else { p.primary };
    let mem_title = format!(" Mem · {}/{} MiB ", sys.mem_used_mib, sys.mem_total_mib);
    f.render_widget(gauge(mem_title, mem_pct, mem_color, p), mem_area);

    match &sys.battery {
        Some((pct, status)) => {
            let color = match *pct {
                0..=19 => p.danger,
                20..=49 => p.highlight,
                _ => p.success,
            };
            f.render_widget(
                gauge(format!(" Power · {status} "), *pct, color, p),
                bat_area,
            );
        }
        None => f.render_widget(
            Paragraph::new(" No battery")
                .style(Style::new().fg(p.dim))
                .block(panel(" Power ", p.dim)),
            bat_area,
        ),
    }

    // Active configuration summary and external binary availability
    let row = |ok: bool, label: &'static str, value: Span<'static>| {
        let dot = if ok {
            val("● ", p.success)
        } else {
            val("✖ ", p.danger)
        };
        Line::from(vec![
            dot,
            Span::styled(
                format!("{label:<16}"),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            value,
        ])
    };
    let tool_span = |installed: bool| {
        if installed {
            val("available in PATH", p.success)
        } else {
            val("missing from PATH", p.danger)
        }
    };
    let cfg = &app.settings.cfg;
    f.render_widget(
        Paragraph::new(vec![
            row(
                true,
                "Storage target",
                val(cfg.download_dir.clone(), p.highlight),
            ),
            row(true, "Active theme", val(cfg.theme.name(), p.secondary)),
            row(
                true,
                "Threads",
                val(cfg.concurrent_fragments.to_string(), p.primary),
            ),
            row(host.has_ytdlp, "yt-dlp", tool_span(host.has_ytdlp)),
            row(host.has_ffmpeg, "ffmpeg", tool_span(host.has_ffmpeg)),
        ])
        .block(panel(" Environment & Dependencies ", p.dim)),
        env,
    );
}

fn render_downloader(f: &mut Frame, app: &App, p: &Palette, area: Rect) {
    use Constraint::{Length, Min};

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
        Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>())
            .style(Style::new().fg(p.highlight))
    };

    match dl.step {
        Step::Url => {
            let [hint, input, _] = Layout::vertical([Length(2), Length(3), Min(0)]).areas(inner);
            let text = if focused {
                "Step 1 of 3 — paste a URL, Enter to continue (Ctrl+U clear · Ctrl+W delete word)"
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
            render_choices(f, list, "Media Type", items, dl.kind_idx, p.primary, p);
        }
        Step::Format => {
            let formats = dl.formats();
            let [head, list, _] =
                Layout::vertical([Length(3), Length(formats.len() as u16 + 2), Min(0)])
                    .areas(inner);
            f.render_widget(
                heading(vec![
                    format!("URL: {}", dl.url),
                    format!("Target directory: {}", app.settings.cfg.download_dir),
                    "Step 3 of 3 — choose a format, Enter to run yt-dlp (h to go back)".into(),
                ]),
                head,
            );
            let items = formats.iter().map(|fmt| format!(" .{fmt}")).collect();
            render_choices(
                f,
                list,
                "Container / Codec",
                items,
                dl.format_idx,
                p.success,
                p,
            );
        }
        Step::Running | Step::Done => {
            let (state, pct, speed, eta, lines) = {
                let job = dl.job.lock().unwrap();
                let keep = if dl.step == Step::Running {
                    inner.height.saturating_sub(6) as usize
                } else {
                    8
                };
                let skip = job.lines.len().saturating_sub(keep);
                let lines: Vec<Line> = job
                    .lines
                    .iter()
                    .skip(skip)
                    .cloned()
                    .map(Line::from)
                    .collect();
                (
                    job.state,
                    job.percent,
                    job.speed.clone(),
                    job.eta.clone(),
                    lines,
                )
            };

            let (headline, color) = match state {
                JobState::Running => ("⟳ Downloading…", p.highlight),
                JobState::Succeeded => ("✔ Download complete", p.success),
                JobState::Failed => ("✖ Download failed", p.danger),
                JobState::Cancelled => ("⊘ Download cancelled", p.dim),
            };

            let bar_title = match (state, speed, eta) {
                (JobState::Running, Some(spd), Some(e)) => {
                    format!(" Progress · {spd} · ETA {e} ")
                }
                (JobState::Running, Some(spd), None) => format!(" Progress · {spd} "),
                _ => " Progress ".into(),
            };

            let [head, bar, log, foot] =
                Layout::vertical([Length(2), Length(3), Min(1), Length(1)]).areas(inner);
            f.render_widget(
                Paragraph::new(headline)
                    .style(Style::new().fg(color).add_modifier(Modifier::BOLD)),
                head,
            );
            f.render_widget(gauge(bar_title, pct as u16, color, p), bar);
            f.render_widget(
                Paragraph::new(lines)
                    .style(Style::new().fg(p.text))
                    .wrap(Wrap { trim: true }),
                log,
            );
            let hint = if state == JobState::Running {
                "c: cancel download · Esc: back to sidebar (download continues)"
            } else {
                "Enter / n: new download · r: retry same URL · Esc: back to sidebar"
            };
            f.render_widget(Paragraph::new(hint).style(Style::new().fg(p.dim)), foot);
        }
    }
}

fn render_analytics(f: &mut Frame, app: &App, p: &Palette, area: Rect) {
    use Constraint::{Length, Min};

    let focused = app.focus == Focus::Content;
    let title = if focused {
        " Session Analytics · Esc to return "
    } else {
        " Session Analytics · Enter to inspect "
    };
    let frame = panel(title, if focused { p.primary } else { p.dim });
    let inner = frame.inner(area);
    f.render_widget(frame, area);

    let ok = app
        .history
        .iter()
        .filter(|h| h.state == JobState::Succeeded)
        .count();
    let cancelled = app
        .history
        .iter()
        .filter(|h| h.state == JobState::Cancelled)
        .count();
    let failed = app.history.len() - ok - cancelled;

    let [summary_a, list_a, foot_a] =
        Layout::vertical([Length(2), Min(1), Length(1)]).areas(inner);

    let summary = Line::from(vec![
        val("✔ ", p.success),
        Span::raw(format!("{ok} completed    ")),
        val("✖ ", p.danger),
        Span::raw(format!("{failed} failed    ")),
        val("⊘ ", p.dim),
        Span::raw(format!("{cancelled} cancelled")),
    ]);
    f.render_widget(Paragraph::new(summary), summary_a);

    if app.history.is_empty() {
        f.render_widget(
            Paragraph::new("Nothing downloaded yet — open Download Media to get started.")
                .style(Style::new().fg(p.dim)),
            list_a,
        );
    } else {
        let items: Vec<Line> = app
            .history
            .iter()
            .rev()
            .map(|h| {
                let (icon, color) = match h.state {
                    JobState::Succeeded => ("✔", p.success),
                    JobState::Cancelled => ("⊘", p.dim),
                    _ => ("✖", p.danger),
                };
                Line::from(vec![
                    val(format!(" {icon} "), color),
                    val(format!(".{:<5} ", h.format), p.highlight),
                    val(format!("{:>3}s  ", h.elapsed_secs), p.dim),
                    Span::styled(h.url.as_str(), Style::new().fg(p.text)),
                ])
            })
            .collect();

        let highlight = if focused {
            Style::new()
                .bg(p.surface)
                .fg(p.highlight)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };

        let list = List::new(items)
            .highlight_style(highlight)
            .highlight_symbol(if focused { "▸" } else { " " });
        let selected = focused.then_some(app.history_idx);
        f.render_stateful_widget(
            list,
            list_a,
            &mut ListState::default().with_selected(selected),
        );
    }

    let foot_text = if focused && !app.history.is_empty() {
        "j/k select · Enter/r retry download · c clear history · Esc back"
    } else {
        "Press Enter from sidebar to select or retry past downloads"
    };
    f.render_widget(
        Paragraph::new(foot_text).style(Style::new().fg(p.dim)),
        foot_a,
    );
}

fn render_settings(f: &mut Frame, app: &App, p: &Palette, area: Rect) {
    use Constraint::{Length, Min, Percentage};

    let focused = app.focus == Focus::Content;
    let s = &app.settings;
    let active = |field: Field| focused && s.field() == field;
    let hint = |t: &'static str| val(t, p.dim);
    let accent =
        |t: String| Span::styled(t, Style::new().fg(p.primary).add_modifier(Modifier::BOLD));

    let frame = panel(
        if focused {
            " Settings · Esc to return "
        } else {
            " Settings · Enter to configure "
        },
        if focused { p.secondary } else { p.dim },
    );
    let inner = frame.inner(area);
    f.render_widget(frame, area);

    let [path_a, theme_a, flags_a, save_a, footer_a] =
        Layout::vertical([Length(3), Length(3), Length(3), Length(2), Min(1)]).areas(inner);
    let [subs_a, threads_a] = Layout::horizontal([Percentage(50), Percentage(50)]).areas(flags_a);

    // Download directory path editor
    let max_path_chars = path_a.width.saturating_sub(30) as usize;
    let path_line = match &s.editing {
        Some(buf) => {
            let shown = tail_chars(buf, max_path_chars.max(10));
            Line::from(vec![
                Span::styled(
                    format!("{shown}█"),
                    Style::new().fg(p.highlight).add_modifier(Modifier::BOLD),
                ),
                hint("   Enter confirm · Esc cancel"),
            ])
        }
        None => Line::from(vec![
            val(s.cfg.download_dir.as_str(), p.text),
            hint("   Enter to edit"),
        ]),
    };
    f.render_widget(
        Paragraph::new(path_line).block(card("Download directory", active(Field::Path), p)),
        path_a,
    );

    // Theme selector with position counter and 5-swatch preview bar
    let theme_idx = Theme::ALL.iter().position(|&t| t == s.cfg.theme).unwrap_or(0) + 1;
    f.render_widget(
        Paragraph::new(Line::from(vec![
            accent("◂ ".into()),
            accent(s.cfg.theme.name().into()),
            accent(" ▸ ".into()),
            val(format!("({theme_idx}/{})  ", Theme::ALL.len()), p.dim),
            val("██", p.primary),
            val("██", p.secondary),
            val("██", p.highlight),
            val("██", p.success),
            val("██", p.danger),
            hint("   h/l to switch"),
        ]))
        .block(card("Colour scheme", active(Field::Theme), p)),
        theme_a,
    );

    // Auto-subtitle toggle
    let subs = if s.cfg.embed_subtitles {
        val("● On", p.success)
    } else {
        val("○ Off", p.dim)
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![subs, hint("   embeds EN subs (video)")]))
            .block(card("Auto subtitles", active(Field::Subtitles), p)),
        subs_a,
    );

    // Concurrent fragment thread count
    f.render_widget(
        Paragraph::new(Line::from(vec![
            accent(format!("◂ {} ▸", s.cfg.concurrent_fragments)),
            hint("   h/l to adjust"),
        ]))
        .block(card("Download threads", active(Field::Threads), p)),
        threads_a,
    );

    // Save button and status message
    let button = if active(Field::Save) {
        Style::new()
            .bg(p.success)
            .fg(ON_ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().bg(p.surface).fg(p.text)
    };
    let status = match (&s.status, s.is_dirty()) {
        (Some((msg, ok)), _) => Span::styled(
            msg.as_str(),
            Style::new()
                .fg(if *ok { p.success } else { p.danger })
                .add_modifier(Modifier::BOLD),
        ),
        (None, true) => val("● Unsaved changes (Enter or Ctrl+S to save)", p.highlight),
        (None, false) => hint("Enter or Ctrl+S to write config to disk"),
    };
    let save_label = if s.is_dirty() {
        " Save settings * "
    } else {
        " Save settings "
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(save_label, button),
            Span::raw("  "),
            status,
        ])),
        save_a,
    );

    f.render_widget(
        Paragraph::new("j/k move · h/l adjust · Enter edit / toggle · Ctrl+S save · Esc back")
            .style(Style::new().fg(p.dim)),
        footer_a,
    );
}

fn render_help(f: &mut Frame, _app: &App, p: &Palette, area: Rect) {
    let section = |t: &'static str| {
        Line::styled(t, Style::new().fg(p.highlight).add_modifier(Modifier::BOLD))
    };
    let key = |k: &'static str, d: &'static str| {
        Line::from(vec![
            Span::styled(format!("  {k:<18}"), Style::new().fg(p.primary)),
            Span::raw(d),
        ])
    };

    let lines = vec![
        section("Anywhere"),
        key("Ctrl+C", "Quit immediately"),
        Line::from(""),
        section("Sidebar"),
        key("j/k  ↑/↓", "Move selection"),
        key("Enter  l  →", "Open panel (or trigger Update & Reboot)"),
        key("r", "Refresh system metrics (on System Overview)"),
        key("Shift+U", "Instant rebuild & restart from anywhere in sidebar"),
        key("q  Esc", "Quit"),
        Line::from(""),
        section("Downloader & Analytics"),
        key("j/k  ↑/↓", "Move between options or history entries"),
        key("Enter", "Confirm, advance, or retry selected download"),
        key("c  x", "Cancel running download / clear analytics history"),
        key("r", "Retry finished or selected download"),
        key("h  ←  Backspace", "Go back / return to sidebar"),
        key("Esc", "Return to the sidebar (active download continues)"),
        Line::from(""),
        section("Settings & Text Fields"),
        key("h/l  ←/→", "Adjust setting value"),
        key("Ctrl+S", "Save configuration to disk"),
        key("Ctrl+U", "Clear text field"),
        key("Ctrl+W", "Delete previous word / path segment"),
    ];

    f.render_widget(
        Paragraph::new(lines)
            .style(Style::new().fg(p.text))
            .block(panel(" Help ", p.dim)),
        area,
    );
}

fn render_update(f: &mut Frame, app: &App, p: &Palette, area: Rect) {
    let dir = project_dir();
    let bin = dir.join("target/release/thinkbot");
    let button = Style::new()
        .bg(p.primary)
        .fg(ON_ACCENT)
        .add_modifier(Modifier::BOLD);

    let mut lines = vec![
        Line::styled(
            " Update, Rebuild & Restart",
            Style::new().fg(p.highlight).add_modifier(Modifier::BOLD),
        ),
        Line::from(""),
        kv("Project", val(dir.display().to_string(), p.text), p),
        kv("Binary", val(bin.display().to_string(), p.text), p),
        Line::from(""),
        Line::styled(
            " Pressing Enter on this sidebar item will:",
            Style::new().fg(p.secondary).add_modifier(Modifier::BOLD),
        ),
        Line::from(vec![
            val("  1. ", p.primary),
            Span::raw("Save any unsaved configuration changes"),
        ]),
        Line::from(vec![
            val("  2. ", p.primary),
            Span::raw("Close the TUI and pull latest git changes (git pull --ff-only)"),
        ]),
        Line::from(vec![
            val("  3. ", p.primary),
            Span::raw("Run cargo build --release and update ~/.local/bin/thinkbot"),
        ]),
        Line::from(vec![
            val("  4. ", p.primary),
            Span::raw("Immediately reboot into the newly compiled thinkbot binary"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw(" "),
            Span::styled(" ↻ Press Enter (or l / →) to Rebuild & Reboot Now ", button),
        ]),
    ];

    if app.settings.is_dirty() {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            " ● Unsaved settings will be automatically saved before rebooting.",
            Style::new().fg(p.highlight),
        ));
    }

    f.render_widget(
        Paragraph::new(lines)
            .style(Style::new().fg(p.text))
            .block(panel(" Update & Reboot ", p.primary)),
        area,
    );
}

// =========================================================================
// Unit Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_cycles_both_directions() {
        assert_eq!(wrap(0, 5, -1), 4);
        assert_eq!(wrap(4, 5, 1), 0);
        assert_eq!(wrap(2, 5, 2), 4);
        assert_eq!(wrap(0, 0, 1), 0);
    }

    #[test]
    fn tail_chars_handles_unicode_and_bounds() {
        assert_eq!(tail_chars("abcdef", 3), "def");
        assert_eq!(tail_chars("abc", 10), "abc");
        assert_eq!(tail_chars("αβγδε", 2), "δε");
        assert_eq!(tail_chars("", 5), "");
    }

    #[test]
    fn format_uptime_formats_days_and_hours() {
        assert_eq!(format_uptime(3661), "1h 1m");
        assert_eq!(format_uptime(90061), "1d 1h 1m");
    }

    #[test]
    fn job_collapses_progress_and_extracts_speed_eta() {
        let mut job = Job::default();
        job.push("[download] Destination: video.mp4");
        job.push("[download]  12.5% of   50.00MiB at    4.20MiB/s ETA 00:10");
        job.push("[download]  45.0% of   50.00MiB at    6.10MiB/s ETA 00:05");

        assert_eq!(job.lines.len(), 2);
        assert!((job.percent - 45.0).abs() < f64::EPSILON);
        assert_eq!(job.speed.as_deref(), Some("6.10MiB/s"));
        assert_eq!(job.eta.as_deref(), Some("00:05"));
    }

    #[test]
    fn edit_text_supports_word_deletion_and_clear() {
        let mut buf = String::from("https://example.com/watch?v=123");
        edit_text(
            &mut buf,
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
        );
        assert_eq!(buf, "https://example.com/watch?");

        edit_text(
            &mut buf,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn theme_cycle_wraps_around() {
        let first = Theme::ALL[0];
        let last = Theme::ALL[Theme::ALL.len() - 1];
        assert_eq!(first.cycle(-1), last);
        assert_eq!(last.cycle(1), first);
    }
}
