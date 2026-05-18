# lyrsync-cli

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL%20v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Rust](https://img.shields.io/badge/rust-2026-orange.svg)](https://www.rust-lang.org)

A *blazingly fast*, zero friction Rust CLI to fetch, convert, and natively embed word-by-word synchronized eLRC lyrics directly into your local audio files.

Whether you want to scan an entire music library to inject lyrics into FLAC metadata, or just convert a local Musixmatch JSON payload, `lyrsync-cli` handles the heavy lifting safely and automatically.

## 🚀 Installation
### 📦 Prebuilt

**Note: macOS users have to compile the program themselves.**
* **Windows / Linux:** None! Just download the prebuilt binary from the [releases](https://github.com/lunishworks/lyrsync-cli/releases) tab.
* **macOS / Manual Compilation:** Rust & Cargo (if compiling from source)


### 🏗️ Compilation

Clone the repository and build the optimized release binary:

```bash
git clone https://github.com/lunishworks/lyrsync-cli.git
cd lyrsync-cli
cargo build --release
```
*(The compiled binary will be located in `target/release/lyrsync-cli`)*

## 🛠️ Usage Examples

`lyrsync-cli` defaults to **eLRC** (word-by-word karaoke style timings). If a song only has standard line-by-line lyrics available, it will automatically and safely fall back to standard **LRC**.

By default, lyrics are **not** written to disk as `.lrc` files — they go straight into the audio metadata via `--embed`. If you also want `.lrc` sidecar files saved alongside your audio, add `--sidecar`.

### The "Magic Wand" (Auto-Tag a Folder)
Scans the current directory, reads the ID3/Vorbis tags (or cleans up the filenames if tags are missing), fetches the best available lyrics, and embeds them directly into the audio files.
```bash
lyrsync-cli --auto --embed
```

Want `.lrc` sidecar files saved too? Just add `--sidecar`:
```bash
lyrsync-cli --auto --embed --sidecar
```

### Fetch by Query
Manually search for a specific track. Without `--sidecar`, nothing is written to disk unless you also pass `--embed`.
```bash
# Embed into audio only
lyrsync-cli --fetch "Radiohead - Fake Plastic Trees" --embed

# Save a .lrc file to the current directory
lyrsync-cli --fetch "Radiohead - Fake Plastic Trees" --sidecar

# Do both
lyrsync-cli --fetch "Radiohead - Fake Plastic Trees" --embed --sidecar
```

### Fetch for a Specific File
Pass a file path and the tool will read the metadata and fetch the lyrics for it.
```bash
lyrsync-cli "C:\Music\Artist - Song.flac" --fetch --embed
```

### Convert Local JSON
If you already have a raw Musixmatch richsync JSON payload, you can convert it to eLRC completely offline.
```bash
# Convert a single file and embed into the matching audio file
lyrsync-cli track_data.json --embed

# Bulk convert every JSON in the directory, saving .lrc sidecars
lyrsync-cli --all --sidecar
```

## ⚙️ Core Flags & Options

| Flag | Description |
| :--- | :--- |
| `--auto` | Automatically scan, fetch, and process all supported audio files in the folder. |
| `--fetch` | Search Musixmatch for lyrics. Can be used empty, with `--all`, or with a `"Artist - Title"` query. |
| `--embed` | Injects the generated lyrics directly into the audio file's native metadata tags. |
| `--sidecar` | Saves a `.lrc` file to disk alongside the audio (or in the current directory). Without this flag, no `.lrc` files are written. |
| `--offset <N>`| Shifts all lyric timestamps by N seconds (e.g., `--offset -1.5` or `--offset 2.0`) to fix bad community syncs. |
| `--lrc` | Forces standard line-synced LRC output, ignoring word-by-word data. |
| `--debug` | Prints verbose extraction and fallback logic for troubleshooting. |
| `--recursive` / `-r` | Recursively scan all subdirectories. |
| `--remove` | Strip embedded lyrics from audio files. |

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

You are free to use, modify, and distribute this software. However, any derivative works including closed-source applications or cloud services utilizing this code must also release their complete source code under the same AGPL-3.0 license.
