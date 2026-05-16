# lyrsync-cli

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL%20v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Rust](https://img.shields.io/badge/rust-2026-orange.svg)](https://www.rust-lang.org)

A *blazingly fast*, zero friction Rust CLI to fetch, convert, and natively embed word-by-word synchronized eLRC lyrics directly into your local audio files. 

Whether you want to scan an entire music library to inject lyrics into FLAC metadata, or just convert a local Musixmatch JSON payload, `lyrsync-cli` handles the heavy lifting safely and automatically.

## ⚡ Prerequisites

To download lyrics directly from the web, `lyrsync-cli` relies on the excellent [musixmatch-cli](https://codeberg.org/ThetaDev/musixmatch-inofficial) backend by ThetaDev. 

You must have it installed and available in your system path to use the `--auto` or `--fetch` flags:
```bash
cargo install musixmatch-cli
```

## 🚀 Installation

Clone the repository and build the optimized release binary:

```bash
git clone https://github.com/lunishworks/lyrsync-cli.git
cd lyrsync-cli
cargo build --release
```
*(The compiled binary will be located in `target/release/lyrsync-cli`)*

## 🛠️ Usage Examples

`lyrsync-cli` defaults to **eLRC** (word-by-word karaoke style timings). If a song only has standard line-by-line lyrics available, it will automatically and safely fall back to standard **LRC**.

### The "Magic Wand" (Auto-Tag a Folder)
Scans the current directory, reads the ID3/Vorbis tags (or cleans up the filenames if tags are missing), fetches the best available lyrics, saves them as `.lrc` files, and embeds them directly into the audio files.
```bash
lyrsync-cli --auto --embed
```

### Fetch by Query
Manually search for a specific track and download the `.lrc` file to your current directory.
```bash
lyrsync-cli --fetch "Radiohead - Fake Plastic Trees"
```

### Fetch for a Specific File
Pass a file path, and the tool will figure out the metadata and fetch the lyrics for it.
```bash
lyrsync-cli "C:\Music\Artist - Song.flac" --fetch --embed
```

### Convert Local JSON
If you already have a raw Musixmatch richsync JSON payload, you can convert it to eLRC completely offline.
```bash
# Convert a single file
lyrsync-cli track_data.json --embed

# Bulk convert every JSON in the directory
lyrsync-cli --all
```

## ⚙️ Core Flags & Options

| Flag | Description |
| :--- | :--- |
| `--auto` | Automatically scan, fetch, and process all supported audio files in the folder. |
| `--fetch` | Search Musixmatch for lyrics. Can be used empty, with `--all`, or with a `"Artist - Title"` query. |
| `--embed` | Injects the generated lyrics directly into the audio file's native metadata tags. |
| `--offset <N>`| Shifts all lyric timestamps by N seconds (e.g., `--offset -1.5` or `--offset 2.0`) to fix bad community syncs. |
| `--lrc` | Forces standard line-synced LRC output, ignoring word-by-word data. |
| `--debug` | Prints verbose extraction and fallback logic for troubleshooting. |

## 🎧 Supported Audio Formats

When `--embed` is used, `lyrsync-cli` safely writes to the exact metadata format expected by modern audio players:

| Extension | Embedded Tag Format |
| :--- | :--- |
| `.flac`, `.ogg`, `.opus` | Vorbis Comments (`LYRICS`) |
| `.mp3` | ID3v2 (USLT) |
| `.m4a`, `.mp4` | MP4 ilst (`©lyr`) |
| `.wav` | Standard ID3 fallback |

## 📜 License

This project is licensed under the **GNU Affero General Public License v3.0 (AGPL-3.0)**. 

You are free to use, modify, and distribute this software. However, any derivative works—including closed-source applications or cloud services utilizing this code—must also release their complete source code under the same AGPL-3.0 license.
