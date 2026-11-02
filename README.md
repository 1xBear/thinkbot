# thinkbot

A lightweight, Linux-based, Ratatui terminal user interface. Functions include theme selection, yt-dlp integration, hardware status, and more.

IMPORTANT: This repo is in early infancy, and may not work on every machine or OS. As a personal project, this is known to work on Arch Linux (wayland).

## Features

- **System Overview:** Live hardware telemetry (CPU usage, load, memory, battery, thermal) polled from `/proc` and `/sys` with cached static system attributes and dependency checks (`yt-dlp`, `ffmpeg`).
- **Media Downloader:** Fast multi-step wizard for `yt-dlp` supporting video and audio formats, live speed/ETA progress, background execution, and instant cancellation.
- **Session Analytics:** Interactive download history with duration tracking, status breakdown, and one-key retry.
- **Theme Engine:** 18 built-in color palettes including Catppuccin (Mocha & Macchiato), Tokyo Night, Nord, Gruvbox Dark, Dracula, Rosé Pine, Kanagawa Wave, Everforest Dark, One Dark Pro, Solarized Dark, Ayu Mirage, Nightfox, Material Ocean, Horizon Dark, Monokai Pro, Synthwave '84, and Cyberpunk Neon.
- **In-App Update & Reboot:** Rebuild and restart the application directly from the sidebar (`↻ Update & Reboot` or `Shift+U`).
- **Persistent Settings:** Saves configuration to `~/.config/thinkbot/config.toml` with unsaved-change tracking.

## Requirements

- **Rust & Cargo** (`1.75+`)
- **yt-dlp** (available in your `$PATH`)
- **ffmpeg** (for media container conversion and audio extraction)

## Installation

```bash
git clone https://github.com/1xBear/thinkbot.git
cd thinkbot
cargo build --release
install -Dm755 target/release/thinkbot ~/.local/bin/thinkbot
```
