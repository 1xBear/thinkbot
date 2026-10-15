# thinkbot

A lightweight Linux terminal dashboard. Functions include theme selection, yt-dlp integration, hardware status, and more.

IMPORTANT: This repo is in early infancy, and may not work on every machine or OS. As a personal project, this is known to work on Arch Linux (wayland).

## Features

- **System Overview:** Live hardware stats (load, memory, battery, thermal) polled directly from `/proc` and `/sys`.
- **Media Downloader:** Fast multi-step wizard for `yt-dlp` supporting video and audio formats.
- **Theme Engine:** Built-in color palettes including Gruvbox Dark, Catppuccin Mocha, Tokyo Night, Nord, and Monokai.
- **Persistent Settings:** Automatically saves configuration to `~/.config/thinkbot/config.toml`.

## Requirements

- **Rust & Cargo** (`1.75+`)
- **yt-dlp** (available in your `$PATH`)
- **ffmpeg** (for media container conversion and audio extraction)

## Installation

```bash
git clone [https://github.com/](https://github.com/)1xBear/thinkbot.git
cd thinkbot
cargo build --release
install -Dm755 target/release/thinkbot ~/.local/bin/thinkbot
