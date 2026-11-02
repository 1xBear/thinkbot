# thinkbot

`thinkbot` is a terminal system monitor and `yt-dlp` front-end written in Rust with [Ratatui](https://ratatui.rs).

> **Note:** This project is in early development and targets Linux systems. It is primarily developed and tested on Arch Linux (Wayland).

## Overview

The interface is split into a persistent navigation sidebar and a main workspace panel:

- **System Overview:** Reads live host telemetry directly from `/proc` and `/sys` (`cpu` jiffies, load average, `MemAvailable`, battery capacity/state, and package thermal sensors) without spawning external shell commands. Static system data (OS release, kernel version, root filesystem type, and desktop session) is resolved once on startup.
- **Media Downloader:** A three-step wizard (`URL -> Media Type -> Container/Codec`) that spawns `yt-dlp` in a background thread, parses progress output for transfer rate and ETA, and allows you to keep browsing other panels while jobs run.
- **Session Analytics:** Tracks completed, failed, and cancelled downloads from the current session with elapsed time and one-key retry.
- **Settings:** Configures target download path, concurrent fragment threads (`-N`), automatic subtitle embedding, and active color palette.
- **Self-Update (`Update & Reboot`):** Runs `git pull --ff-only` and `cargo build --release` from the source tree, atomically updates `~/.local/bin/thinkbot`, and replaces the running process via `exec()`.

## Requirements

- **Linux** (relies on `/proc` and `/sys` virtual filesystems)
- **Rust toolchain** (`1.75+`)
- **`yt-dlp`** in `$PATH`
- **`ffmpeg`** in `$PATH` (required by `yt-dlp` for container merging and audio extraction)

## Building and Installation

```bash
git clone https://github.com/1xBear/thinkbot.git
cd thinkbot
cargo build --release
install -Dm755 target/release/thinkbot ~/.local/bin/thinkbot
```

Make sure `~/.local/bin` is in your `$PATH`, then run:

```bash
thinkbot
```

## Keybindings

| Context | Key | Action |
| :--- | :--- | :--- |
| **Global** | `Ctrl+C` | Exit immediately and terminate any active `yt-dlp` child process |
| **Sidebar** | `j` / `k` or `Up` / `Down` | Navigate menu items |
| | `Enter` / `l` / `Right` | Focus the selected panel (or trigger `Update & Reboot`) |
| | `r` | Force an immediate telemetry & dependency check (on System Overview) |
| | `Shift+U` | Rebuild from source and restart in-place |
| | `q` / `Esc` | Quit |
| **Downloader** | `j` / `k` or `Up` / `Down` | Select media type or output container format |
| | `Enter` | Advance to next step or start download |
| | `h` / `Left` / `Backspace` | Return to previous wizard step |
| | `c` / `x` | Cancel an in-progress download |
| | `r` | Re-run the last download with the same URL and format |
| | `n` | Reset wizard for a new URL |
| | `Esc` | Return focus to sidebar (active downloads continue in background) |
| **Analytics** | `j` / `k` or `Up` / `Down` | Select a session history entry |
| | `Enter` / `r` | Load the selected entry back into the downloader |
| | `c` / `x` | Clear session history |
| **Settings** | `j` / `k` or `Up` / `Down` | Move between configuration cards |
| | `h` / `l` or `Left` / `Right` | Cycle color theme, toggle subtitles, or adjust thread count |
| | `Enter` | Edit download path, toggle current field, or save when on Save button |
| | `Ctrl+S` | Write configuration to disk from any setting field |
| **Text Input** | `Ctrl+U` | Clear entire input line |
| | `Ctrl+W` / `Ctrl+Backspace` | Delete previous word or URL/path segment |

## Configuration

Settings are stored at `$XDG_CONFIG_HOME/thinkbot/config.toml` (defaults to `~/.config/thinkbot/config.toml`). Missing fields fall back to internal defaults.

```toml
download_dir = "~/Downloads"
theme = "GruvboxDark"
symbols = "Geometric"
embed_subtitles = false
concurrent_fragments = 4
```

### Available Themes

`CatppuccinMocha`, `CatppuccinMacchiato`, `TokyoNight`, `Nord`, `GruvboxDark`, `Dracula`, `RosePine`, `Kanagawa`, `Everforest`, `OneDark`, `SolarizedDark`, `AyuMirage`, `Nightfox`, `MaterialOcean`, `HorizonDark`, `Monokai`, `Synthwave84`, `Cyberpunk`.

### Available Button Symbol Presets

- `Geometric` (`◈ ▼ ▲ ◆ ? ↻`)
- `Arrows` (`» ↓ ↑ ≡ i ↺`)
- `Technical` (`⠿ ⤓ ⧠ ⚙ ℹ ⥁`)
- `Minimal` (`~ + % * ? @`)
- `NerdFont` (Nerd Font glyphs for desktop, download, chart, cog, help, and refresh)
