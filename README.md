# richsync2elrc

`richsync2elrc` fetches lyrics from Musixmatch, writes a `.lrc` file next to your song, and can optionally embed lyrics into the audio file.

It supports both:

- **eLRC** (word-by-word karaoke style)
- **LRC** (line-by-line)

## Quick start (most common commands)

```bash
# 1) Auto mode: scan this folder, fetch lyrics for all songs, save sidecar .lrc files
richsync2elrc --auto --elrc

# 2) Same as above, but also embed lyrics into audio tags
richsync2elrc --auto --elrc --embed

# 3) Fetch for all songs in this folder (explicit fetch mode)
richsync2elrc --fetch --all --lrc

# 4) Fetch by query
richsync2elrc --fetch "Artist - Song Title" --elrc
```

## eLRC vs LRC

- `--elrc`: prefers richsync (word-level timing). If unavailable, falls back to regular LRC.
- `--lrc`: prefers standard line-synced subtitles. If unavailable, falls back to richsync converted into regular LRC.

If you do not pass either flag, default is `--elrc`.

## Fetch without renaming files

When fetching from files (`--auto`, `--fetch --all`, `FILE --fetch`, or `--fetch` with one song in folder), the app tries:

1. **Audio metadata first** (`artist` + `title`)
2. **Filename cleanup fallback** (track number/punctuation/bracket cleanup)

So users usually do **not** need to rename files manually.

## Main usage modes

### 1) Automatic folder pipeline

```bash
richsync2elrc --auto --elrc
```

- Scans current folder for: `.flac`, `.mp3`, `.opus`, `.m4a`, `.wav`, `.ogg`
- Fetches lyrics
- Writes sidecar `.lrc`
- Embeds only if `--embed` is present

`--auto-tag` still works as an alias of `--auto`.

### 2) Fetch mode

```bash
# Query
richsync2elrc --fetch "Artist - Title" --lrc

# All songs in current folder
richsync2elrc --fetch --all --elrc

# Single song file
richsync2elrc "C:\Music\My Song.flac" --fetch --elrc
```

You can also run `--fetch` with no query:

- If exactly one supported audio file exists in current folder, it uses that file.
- If multiple files exist, it asks you to use `--fetch --all` or specify a file/query.

### 3) Local JSON conversion mode

```bash
# Convert one JSON file
richsync2elrc song.json --elrc

# Convert all JSON files in current folder
richsync2elrc --all --lrc
```

## Embedding details (`--embed`)

When embedding is enabled, lyrics are written to native tag formats:

| Audio format | Tag used |
| --- | --- |
| MP3 | ID3v2 (USLT) |
| FLAC / Opus / Ogg | Vorbis Comments (`LYRICS`) |
| M4A / MP4 | MP4 ilst (`©lyr`) |

## Useful flags

- `--debug`: extra logs (**works with all modes**, including `--auto` / `--auto-tag` / `--fetch`)
- `--offset -1.5`: shift generated timestamps (useful when sync feels early/late)
- `--embed`: write lyrics into audio metadata (in addition to sidecar `.lrc`)

## Build

```bash
cargo build
```
