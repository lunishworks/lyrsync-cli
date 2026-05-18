use clap::Parser;
use musixmatch_inofficial::{
    models::{RichsyncLine, SubtitleFormat, TrackId},
    Musixmatch,
};
use serde::Deserialize;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;

use lofty::config::WriteOptions;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, ItemValue, Tag, TagItem, TagType};

const SUPPORTED_AUDIO_EXTS: [&str; 6] = ["flac", "mp3", "opus", "m4a", "wav", "ogg"];

// Whether we're generating word-by-word eLRC or plain line-synced LRC.
// eLRC is always preferred; LRC is the fallback or forced via --lrc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LyricsMode {
    Elrc,
    Lrc,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None, arg_required_else_help = true)]
struct Args {
    #[arg(name = "FILE")]
    file: Option<PathBuf>,

    /// Process all files in the current directory
    #[arg(short, long)]
    all: bool,

    /// Automatically scan, fetch, and tag all audio files in the current directory
    #[arg(long = "auto", alias = "auto-tag")]
    auto: bool,

    /// Force standard line-synced LRC output instead of word-by-word eLRC
    #[arg(long, conflicts_with = "elrc")]
    lrc: bool,

    /// Force word-by-word eLRC output (this is the default, flag is mostly for clarity)
    #[arg(long, conflicts_with = "lrc")]
    elrc: bool,

    /// Embed the fetched/converted lyrics directly into the audio file's metadata tags
    #[arg(short, long)]
    embed: bool,

    /// Fetch lyrics from Musixmatch. Pass a query ("Artist - Title"), a FILE, --all, or nothing to auto-detect
    #[arg(short, long, num_args = 0..=1, default_missing_value = "")]
    fetch: Option<String>,

    /// Recursively scan all subdirectories for files
    #[arg(short = 'r', long)]
    recursive: bool,

    /// Strip embedded lyrics from audio files
    #[arg(long)]
    remove: bool,

    /// Print verbose debug output (API calls, fallback logic, tag type selection, etc.)
    #[arg(long)]
    debug: bool,

    /// Shift all lyric timestamps by N seconds. Useful for fixing off-sync community lyrics
    #[arg(long, allow_hyphen_values = true)]
    offset: Option<f64>,

    /// Save a .lrc sidecar file to disk. Without this, lyrics only go into audio metadata (via --embed)
    #[arg(long)]
    sidecar: bool,
}

// These map directly to the JSON fields in a Musixmatch richsync payload.
// "c" is the word text, "o" is its offset in seconds from the line's start time.
#[derive(Deserialize, Debug)]
struct Word {
    c: String,
    o: f64,
}

// A single line from the richsync JSON:
//   ts = line start time, te = line end time, x = plain text, l = word-level timing data
// "l" is optional because not every track has word-level data.
#[derive(Deserialize, Debug)]
struct Line {
    ts: f64,
    #[serde(default)]
    te: f64,
    #[serde(default)]
    l: Option<Vec<Word>>,
    x: String,
}

fn selected_lyrics_mode(args: &Args) -> LyricsMode {
    if args.lrc { LyricsMode::Lrc } else { LyricsMode::Elrc }
}

fn format_time(seconds: f64) -> String {
    // LRC strictly expects [mm:ss.xx] with exactly two decimal places.
    // We round to integer hundredths first to avoid floats like 59.999999 bleeding into the next minute.
    let safe_seconds = seconds.max(0.0);
    let total_hundredths = (safe_seconds * 100.0).round() as u64;

    let mins = total_hundredths / 6000;
    let secs = (total_hundredths / 100) % 60;
    let hundredths = total_hundredths % 100;

    format!("{:02}:{:02}.{:02}", mins, secs, hundredths)
}

fn normalize_string(input: &str) -> String {
    // Strips accents and non-alphanumeric characters so we can do fuzzy filename matching.
    // Without this, "Björk - Jóga.flac" would never match against "Bjork - Joga.json".
    input
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'â' | 'á' | 'ä' | 'à' | 'ã' => 'a',
            'î' | 'í' | 'ı' | 'ï' | 'ì' => 'i',
            'û' | 'ú' | 'ü' | 'ù' => 'u',
            'ö' | 'ó' | 'ò' | 'õ' => 'o',
            'ş' | 'ș' | 'š' => 's',
            'ç' | 'ć' | 'č' => 'c',
            'ğ' => 'g',
            'ñ' => 'n',
            _ => c,
        })
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn is_supported_audio_file(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| SUPPORTED_AUDIO_EXTS.contains(&ext.to_lowercase().as_str()))
}

fn strip_bracketed_sections(input: &str) -> String {
    // Drops things like "(Acoustic Version)" or "[2011 Remaster]" from titles.
    // These kill Musixmatch search results because the API won't find an exact match.
    let mut output = String::with_capacity(input.len());
    let mut paren_depth = 0usize;
    let mut square_depth = 0usize;

    for ch in input.chars() {
        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => square_depth += 1,
            ']' => square_depth = square_depth.saturating_sub(1),
            _ if paren_depth == 0 && square_depth == 0 => output.push(ch),
            _ => {}
        }
    }
    output
}

// Cleans up a raw artist or title string pulled from a filename.
// Strips bracketed junk, collapses underscores and extra spaces, trims punctuation from edges.
fn clean_filename_component(input: &str) -> String {
    strip_bracketed_sections(input)
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_string()
}

// Splits a "Artist - Title" string into its two parts. Handles the most common dash variants
// people actually use in filenames (en dash, em dash, plain hyphen).
fn parse_artist_title_query(input: &str) -> Option<(String, String)> {
    let trimmed = input.trim();
    let (raw_artist, raw_title) = [" - ", " \u{2013} ", " \u{2014} ", "-"]
        .iter()
        .find_map(|sep| trimmed.split_once(sep))?;

    let artist = clean_filename_component(raw_artist);
    let title = clean_filename_component(raw_title);

    if artist.is_empty() || title.is_empty() {
        return None;
    }
    Some((artist, title))
}

// Converts a raw Musixmatch richsync JSON payload (our custom Line/Word structs) into an LRC string.
// If word-level data ("l") exists and we're not forced to plain LRC, we output eLRC with <timestamp>Word tags.
// Otherwise we fall back to a plain [timestamp]Line per line.
fn process_json_to_elrc(
    content: &str, force_lrc: bool, offset_val: f64, debug: bool, log: &mut String,
) -> Option<String> {
    let Ok(lines) = serde_json::from_str::<Vec<Line>>(content) else {
        if debug { writeln!(log, "[DEBUG] Serde parsing failed.").unwrap(); }
        return None;
    };

    let mut output = String::new();
    for line in lines {
        let shifted_ts = line.ts + offset_val;
        output.push_str(&format!("[{}]", format_time(shifted_ts)));

        if !force_lrc && line.l.is_some() {
            // eLRC: emit each word with its absolute timestamp, then close the line with the end time
            for word in line.l.as_ref().unwrap() {
                if word.c.trim().is_empty() {
                    output.push_str(&word.c); // preserve spaces between words as-is
                } else {
                    output.push_str(&format!("<{}>{}", format_time(shifted_ts + word.o), word.c));
                }
            }
            output.push_str(&format!("<{}>\n", format_time(line.te + offset_val)));
        } else {
            // Plain LRC: just the line text, no word-level tags
            output.push_str(&format!("{}\n", line.x));
        }
    }

    (!output.trim().is_empty()).then_some(output)
}

// Same rendering logic as process_json_to_elrc but for the RichsyncLine type returned
// directly by the musixmatch-inofficial crate instead of our own deserialized structs.
fn render_richsync_lines(lines: &[RichsyncLine], force_lrc: bool, offset_val: f64) -> Option<String> {
    let mut output = String::new();

    for line in lines {
        let shifted_ts = f64::from(line.ts) + offset_val;
        output.push_str(&format!("[{}]", format_time(shifted_ts)));

        if !force_lrc && !line.l.is_empty() {
            for word in &line.l {
                if word.c.trim().is_empty() {
                    output.push_str(&word.c);
                } else {
                    output.push_str(&format!("<{}>{}", format_time(shifted_ts + f64::from(word.o)), word.c));
                }
            }
            output.push_str(&format!("<{}>\n", format_time(f64::from(line.te) + offset_val)));
        } else {
            output.push_str(&format!("{}\n", line.x));
        }
    }

    (!output.trim().is_empty()).then_some(output)
}

// Tries to fetch richsync data (word-by-word timing) from Musixmatch and render it as eLRC.
// This is the highest-quality lyrics source; we always try this before falling back to plain LRC.
// Set force_lrc=true to strip out the word tags and get plain LRC from richsync timing data instead.
async fn fetch_richsync_converted(
    musixmatch: &Musixmatch, artist: &str, title: &str,
    force_lrc: bool, offset_val: f64, debug: bool, log: &mut String,
) -> Option<String> {
    let Ok(track) = musixmatch.matcher_track(title, artist, "", false, false, false).await else {
        if debug { writeln!(log, "[DEBUG] matcher_track failed").unwrap(); }
        return None;
    };

    let Ok(richsync) = musixmatch.track_richsync(TrackId::TrackId(track.track_id), None, None).await else {
        if debug { writeln!(log, "[DEBUG] track_richsync failed").unwrap(); }
        return None;
    };

    let Ok(lines) = richsync.get_lines() else {
        if debug { writeln!(log, "[DEBUG] richsync line parse failed").unwrap(); }
        return None;
    };

    render_richsync_lines(&lines, force_lrc, offset_val)
}

// Fetches a plain line-synced LRC subtitle from Musixmatch's subtitle endpoint.
// Less detailed than richsync but available for far more tracks.
// We do a basic sanity check for bracket characters to filter out obviously blank/broken responses.
async fn fetch_standard_lrc(
    musixmatch: &Musixmatch, artist: &str, title: &str, debug: bool, log: &mut String,
) -> Option<String> {
    let Ok(subtitle) = musixmatch.matcher_subtitle(title, artist, SubtitleFormat::Lrc, None, None).await else {
        if debug { writeln!(log, "[DEBUG] matcher_subtitle failed").unwrap(); }
        return None;
    };

    let trimmed = subtitle.subtitle_body.trim();
    if trimmed.contains('[') && trimmed.contains(']') {
        Some(trimmed.to_string())
    } else {
        None
    }
}

// Each audio format stores lyrics in a different metadata container.
// Lofty supports writing all of them, but we have to ask for the right one explicitly.
fn preferred_lyrics_tag_type(file_type: FileType) -> TagType {
    match file_type {
        FileType::Mpeg => TagType::Id3v2,
        FileType::Flac | FileType::Opus | FileType::Vorbis | FileType::Speex => TagType::VorbisComments,
        FileType::Mp4 => TagType::Mp4Ilst,
        _ => file_type.primary_tag_type(),
    }
}

fn embed_into_audio(audio_path: &Path, lyrics: &str, debug: bool, log: &mut String) {
    if debug { writeln!(log, "[DEBUG] Attempting to embed into: {}", audio_path.display()).unwrap(); }

    let Ok(mut tagged_file) = Probe::open(audio_path).and_then(|p| p.read()) else {
        writeln!(log, "  -> Failed to open audio file {}", audio_path.display()).unwrap();
        return;
    };

    let file_type = tagged_file.file_type();
    let tag_type = preferred_lyrics_tag_type(file_type);

    if !tagged_file.supports_tag_type(tag_type) {
        writeln!(log, "  -> {} does not support {:?} lyric tags.", audio_path.display(), tag_type).unwrap();
        return;
    }

    // Clear existing lyrics from all tag types before writing.
    // Lofty can silently produce duplicate tags if we don't wipe first.
    for existing_tag_type in [TagType::Id3v2, TagType::VorbisComments, TagType::Mp4Ilst] {
        if let Some(existing_tag) = tagged_file.tag_mut(existing_tag_type) {
            existing_tag.remove_key(&ItemKey::Lyrics);
        }
    }

    if tagged_file.tag(tag_type).is_none() {
        tagged_file.insert_tag(Tag::new(tag_type));
    }

    if let Some(tag) = tagged_file.tag_mut(tag_type) {
        tag.insert(TagItem::new(ItemKey::Lyrics, ItemValue::Text(lyrics.to_string())));
        if debug { writeln!(log, "[DEBUG] Embedded lyrics using {:?}", tag_type).unwrap(); }
    }

    if let Err(e) = tagged_file.save_to_path(audio_path, WriteOptions::default()) {
        writeln!(log, "  -> Failed to save metadata: {}. Make sure the file isn't open elsewhere!", e).unwrap();
    } else {
        writeln!(log, "  -> Successfully embedded lyrics into metadata!").unwrap();
    }
}

// Wipes the LYRICS tag from an audio file without touching the audio stream itself.
// Checks all three major tag formats since files can have mixed tags from different taggers.
fn remove_lyrics_from_audio(audio_path: &Path, debug: bool, log: &mut String) {
    let Ok(mut tagged_file) = Probe::open(audio_path).and_then(|p| p.read()) else {
        writeln!(log, "  -> Failed to open audio file {}", audio_path.display()).unwrap();
        return;
    };

    let mut removed = false;

    // Lofty won't let us iterate over all tags mutably, so we check each known type explicitly.
    for tag_type in [TagType::Id3v2, TagType::VorbisComments, TagType::Mp4Ilst] {
        if let Some(tag) = tagged_file.tag_mut(tag_type) {
            tag.remove_key(&ItemKey::Lyrics); // returns () in this crate version, can't use return value
            if debug { writeln!(log, "[DEBUG] Stripped lyrics key from {:?}", tag_type).unwrap(); }
            removed = true;
        }
    }

    if removed {
        if let Err(e) = tagged_file.save_to_path(audio_path, WriteOptions::default()) {
            writeln!(log, "  -> Failed to save metadata: {}", e).unwrap();
        } else {
            writeln!(log, "  -> Successfully purged embedded lyrics!").unwrap();
        }
    } else {
        writeln!(log, "  -> No metadata tags found to clean.").unwrap();
    }
}

// Main fetch strategy: try richsync first (word-by-word eLRC), fall back to plain subtitle LRC,
// and as a last resort try to extract plain LRC from richsync timing data.
// The final fallback matters for tracks that have richsync but no subtitle endpoint response.
async fn fetch_lyrics_auto(
    musixmatch: &Musixmatch, artist: &str, title: &str,
    lyrics_mode: LyricsMode, offset_val: f64, debug: bool, log: &mut String,
) -> Option<String> {
    writeln!(log, "  -> Searching Musixmatch for: {} - {}", artist, title).unwrap();

    if lyrics_mode == LyricsMode::Elrc {
        if let Some(elrc) = fetch_richsync_converted(musixmatch, artist, title, false, offset_val, debug, log).await {
            writeln!(log, "  -> Word-by-word eLRC generated.").unwrap();
            return Some(elrc);
        }
        writeln!(log, "  -> Richsync unavailable. Falling back to standard LRC...").unwrap();
    }

    if let Some(lrc) = fetch_standard_lrc(musixmatch, artist, title, debug, log).await {
        writeln!(log, "  -> Standard LRC found.").unwrap();
        return Some(lrc);
    }

    // Only try richsync-as-LRC when we're in LRC mode; in eLRC mode we already tried it above.
    if lyrics_mode == LyricsMode::Lrc {
        writeln!(log, "  -> Standard subtitles unavailable. Attempting to extract LRC from richsync data...").unwrap();
        if let Some(fallback) = fetch_richsync_converted(musixmatch, artist, title, true, offset_val, debug, log).await {
            writeln!(log, "  -> LRC recovered from richsync.").unwrap();
            return Some(fallback);
        }
    }

    writeln!(log, "  -> No lyrics found.").unwrap();
    None
}

// Tries to extract artist/title by splitting the filename on a dash separator.
// We strip leading track numbers first (e.g. "01 - Artist - Title" -> "Artist - Title").
fn parse_filename_for_tags(stem: &str, debug: bool, log: &mut String) -> Option<(String, String)> {
    let without_prefix = stem.trim_start_matches(
        |c: char| c.is_ascii_digit() || c == '.' || c == '-' || c == '_' || c.is_whitespace(),
    );
    let (raw_artist, raw_title) = [" - ", " \u{2013} ", " \u{2014} ", "-"]
        .iter()
        .find_map(|sep| without_prefix.split_once(sep))?;

    let artist = clean_filename_component(raw_artist);
    let title = clean_filename_component(raw_title);

    if artist.is_empty() || title.is_empty() {
        if debug { writeln!(log, "[DEBUG] Filename '{}' produced empty tags after cleanup.", stem).unwrap(); }
        return None;
    }

    Some((artist, title))
}

// Reads embedded ID3/Vorbis/MP4 tags first. Falls back to splitting the filename if
// metadata is missing or incomplete, which is common for files ripped without a tagger.
fn get_artist_and_title(audio_path: &Path, debug: bool, log: &mut String) -> Option<(String, String)> {
    if let Ok(tagged_file) = Probe::open(audio_path).and_then(|p| p.read()) {
        if let Some(tag) = tagged_file.primary_tag().or_else(|| tagged_file.first_tag()) {
            if let (Some(a), Some(t)) = (tag.artist().map(|s| s.into_owned()), tag.title().map(|s| s.into_owned())) {
                return Some((a, t));
            }
        }
    }

    let stem = audio_path.file_stem().unwrap_or_default().to_string_lossy();
    if debug { writeln!(log, "[DEBUG] Metadata missing, falling back to filename: '{}'", stem).unwrap(); }
    parse_filename_for_tags(&stem, debug, log)
}

// Iterative directory walker. We avoid real recursion (and the walkdir crate) by maintaining
// a stack of directories to visit. Sorting the results keeps output order predictable.
fn collect_files<F>(dir: &Path, recursive: bool, filter: F) -> Vec<PathBuf>
where
    F: Fn(&Path) -> bool,
{
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];

    while let Some(current_dir) = dirs.pop() {
        if let Ok(entries) = fs::read_dir(&current_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if recursive { dirs.push(path); }
                } else if filter(&path) {
                    files.push(path);
                }
            }
        }
    }

    files.sort();
    files
}

fn collect_audio_files(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    collect_files(dir, recursive, is_supported_audio_file)
}

fn collect_json_files(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    collect_files(dir, recursive, |p| p.extension().is_some_and(|s| s == "json"))
}

// Fuzzy-matches a base name (e.g. a JSON stem) against audio files in the directory.
// Uses normalize_string on both sides so accents and punctuation don't break the match.
// We check both directions (audio contains base AND base contains audio) to handle
// cases where the JSON name is shorter or longer than the audio filename.
fn find_matching_audio_file(
    base_name: &str, search_dir: &Path, recursive: bool, debug: bool, log: &mut String,
) -> Option<PathBuf> {
    let normalized_base = normalize_string(base_name);
    if normalized_base.is_empty() { return None; }

    collect_audio_files(search_dir, recursive).into_iter().find(|path| {
        let normalized_audio = normalize_string(&path.file_stem().unwrap_or_default().to_string_lossy());
        let matches = normalized_audio.contains(&normalized_base) || normalized_base.contains(&normalized_audio);
        if matches && debug { writeln!(log, "[DEBUG] Matched '{}' for target '{}'", path.display(), base_name).unwrap(); }
        matches
    })
}

// Central output dispatcher. Saves a .lrc sidecar if --sidecar was passed,
// then embeds into audio metadata if --embed was passed.
// Either, both, or neither can be active — the two flags are fully independent.
fn write_lyrics_outputs(
    lyrics: &str, base_name: &str, embed: bool, sidecar: bool,
    current_dir: &Path, explicit_audio_path: Option<&Path>,
    recursive: bool, debug: bool, log: &mut String,
) {
    if sidecar {
        let lrc_path = current_dir.join(format!("{}.lrc", base_name));
        if let Err(e) = fs::write(&lrc_path, lyrics) {
            writeln!(log, "  -> Failed to write sidecar file: {}", e).unwrap();
        } else {
            writeln!(log, "  -> Saved sidecar file to {}", lrc_path.display()).unwrap();
        }
    }

    if !embed { return; }

    // If we were given an explicit audio path (e.g. user passed a FILE), use it directly.
    // Otherwise do a fuzzy search to find a matching audio file next to the JSON.
    let target_audio = explicit_audio_path
        .map(Path::to_path_buf)
        .or_else(|| find_matching_audio_file(base_name, current_dir, recursive, debug, log));

    if let Some(audio_path) = target_audio {
        embed_into_audio(&audio_path, lyrics, debug, log);
    } else {
        writeln!(log, "  -> No matching audio file found to embed into.").unwrap();
    }
}

// Entry point for the local JSON conversion path (as opposed to the API fetch path).
// Buffers log output and prints it in one shot to avoid garbled output when running in parallel.
fn convert_and_embed(
    content: &str, base_name: &str, force_lrc: bool, embed: bool, sidecar: bool,
    current_dir: &Path, recursive: bool, debug: bool, offset_val: f64,
) {
    let mut log = String::new();
    writeln!(&mut log, "--------------------------------------------------").unwrap();
    writeln!(&mut log, "Converting JSON: {}", base_name).unwrap();

    if let Some(output) = process_json_to_elrc(content, force_lrc, offset_val, debug, &mut log) {
        write_lyrics_outputs(&output, base_name, embed, sidecar, current_dir, None, recursive, debug, &mut log);
    }

    print!("{}", log);
}

async fn remove_for_audio_file(audio_path: PathBuf, debug: bool) {
    let mut log = String::new();
    writeln!(&mut log, "--------------------------------------------------").unwrap();
    writeln!(&mut log, "Processing: {}", audio_path.display()).unwrap();

    remove_lyrics_from_audio(&audio_path, debug, &mut log);

    print!("{}", log);
}

async fn remove_for_audio_directory(dir: &Path, recursive: bool, debug: bool) {
    let audio_files = collect_audio_files(dir, recursive);
    if audio_files.is_empty() {
        eprintln!("Error: No supported audio files found in {}.", dir.display());
        return;
    }

    // Local I/O is fast so we can run a lot of concurrent tasks.
    // Cap at 50 anyway — beyond that the OS starts throwing "too many open files".
    let semaphore = Arc::new(Semaphore::new(50));
    let mut handles = Vec::new();

    for path in audio_files {
        let sem_clone = Arc::clone(&semaphore);
        handles.push(tokio::spawn(async move {
            let _permit = sem_clone.acquire().await.unwrap();
            remove_for_audio_file(path, debug).await;
        }));
    }

    for handle in handles { let _ = handle.await; }
}

async fn fetch_for_audio_file(
    musixmatch: Arc<Musixmatch>, audio_path: PathBuf, lyrics_mode: LyricsMode,
    embed: bool, sidecar: bool, offset_val: f64, debug: bool, recursive: bool,
) {
    // Buffer everything into a String and print it at the end.
    // If we wrote directly to stdout, concurrent tasks would interleave their output randomly.
    let mut log = String::new();
    writeln!(&mut log, "--------------------------------------------------").unwrap();
    writeln!(&mut log, "Processing: {}", audio_path.display()).unwrap();

    let Some((artist, title)) = get_artist_and_title(&audio_path, debug, &mut log) else {
        writeln!(&mut log, "  -> Could not determine artist/title, skipping.").unwrap();
        print!("{}", log);
        return;
    };

    let Some(lyrics_text) = fetch_lyrics_auto(&musixmatch, &artist, &title, lyrics_mode, offset_val, debug, &mut log).await else {
        print!("{}", log);
        return;
    };

    let stem = audio_path.file_stem().unwrap_or_default().to_string_lossy();
    let output_dir = audio_path.parent().unwrap_or_else(|| Path::new("."));
    write_lyrics_outputs(&lyrics_text, &stem, embed, sidecar, output_dir, Some(&audio_path), recursive, debug, &mut log);

    print!("{}", log);
}

async fn fetch_for_audio_directory(
    musixmatch: Arc<Musixmatch>, dir: &Path, lyrics_mode: LyricsMode,
    embed: bool, sidecar: bool, offset_val: f64, debug: bool, recursive: bool,
) {
    let audio_files = collect_audio_files(dir, recursive);
    if audio_files.is_empty() {
        eprintln!("Error: No supported audio files found in {}.", dir.display());
        return;
    }

    // 5 concurrent API requests is conservative but safe. Musixmatch will rate-limit or
    // temporarily block IPs that hammer the endpoint too aggressively.
    let semaphore = Arc::new(Semaphore::new(5));
    let mut handles = Vec::new();

    for path in audio_files {
        let mx_clone = Arc::clone(&musixmatch);
        let sem_clone = Arc::clone(&semaphore);

        handles.push(tokio::spawn(async move {
            let _permit = sem_clone.acquire().await.unwrap();
            fetch_for_audio_file(mx_clone, path, lyrics_mode, embed, sidecar, offset_val, debug, recursive).await;
        }));
    }

    for handle in handles { let _ = handle.await; }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let offset_val = args.offset.unwrap_or(0.0);
    let lyrics_mode = selected_lyrics_mode(&args);
    let force_lrc = matches!(lyrics_mode, LyricsMode::Lrc);
    let recursive = args.recursive;
    let sidecar = args.sidecar;

    // --remove short-circuits everything else — we don't want to accidentally fetch
    // or convert anything when the user just wants to strip lyrics out.
    if args.remove {
        if args.auto || args.all {
            println!("Starting automatic lyric removal pipeline...");
            remove_for_audio_directory(Path::new("."), recursive, args.debug).await;
        } else if let Some(file_path) = args.file {
            if file_path.exists() && is_supported_audio_file(&file_path) {
                remove_for_audio_file(file_path, args.debug).await;
            } else {
                eprintln!("Error: FILE must be a valid audio file when used with --remove.");
            }
        } else {
            eprintln!("Error: Use --remove with --auto, --all, or provide a specific FILE.");
        }
        return;
    }

    // Only spin up the Musixmatch client if we actually need it.
    // Building it reads credentials from the environment, so we skip it for pure JSON conversions.
    let should_fetch = args.auto || args.fetch.is_some();
    let musixmatch = if should_fetch {
        match Musixmatch::builder().build() {
            Ok(client) => Some(Arc::new(client)),
            Err(e) => {
                eprintln!("Error: Could not initialize Musixmatch client: {}", e);
                return;
            }
        }
    } else {
        None
    };

    if args.auto {
        let Some(musixmatch) = musixmatch else { return; };
        println!("Starting automatic audio fetch pipeline...");
        fetch_for_audio_directory(musixmatch, Path::new("."), lyrics_mode, args.embed, sidecar, offset_val, args.debug, recursive).await;
        return;
    }

    if let Some(fetch_arg) = args.fetch.as_deref() {
        let Some(musixmatch) = musixmatch else { return; };

        if args.all {
            println!("Starting fetch pipeline for all audio files...");
            fetch_for_audio_directory(musixmatch, Path::new("."), lyrics_mode, args.embed, sidecar, offset_val, args.debug, recursive).await;
            return;
        }

        let query = fetch_arg.trim();
        if !query.is_empty() {
            // User passed an explicit "Artist - Title" query string
            let mut log = String::new();
            let Some((artist, title)) = parse_artist_title_query(query) else {
                eprintln!("Error: --fetch query must be in \"Artist - Title\" format.");
                return;
            };

            if let Some(lyrics) = fetch_lyrics_auto(&musixmatch, &artist, &title, lyrics_mode, offset_val, args.debug, &mut log).await {
                let clean_title = clean_filename_component(&title);
                let base_name = if clean_title.is_empty() { &title } else { &clean_title };
                write_lyrics_outputs(&lyrics, base_name, args.embed, sidecar, Path::new("."), None, recursive, args.debug, &mut log);
            }
            print!("{}", log);
            return;
        }

        // --fetch with no query: use FILE if provided, otherwise auto-detect if there's only one audio file around
        if let Some(file_path) = args.file {
            if !file_path.exists() || !is_supported_audio_file(&file_path) {
                eprintln!("Error: FILE must be a valid audio file when used with --fetch.");
                return;
            }
            fetch_for_audio_file(musixmatch, file_path, lyrics_mode, args.embed, sidecar, offset_val, args.debug, recursive).await;
            return;
        }

        let audio_files = collect_audio_files(Path::new("."), recursive);
        if audio_files.is_empty() {
            eprintln!("Error: No supported audio files found. Use --fetch \"Artist - Title\" or --fetch --all.");
        } else if audio_files.len() == 1 {
            println!("No fetch query provided; using the only audio file in the current directory.");
            fetch_for_audio_file(musixmatch, audio_files[0].clone(), lyrics_mode, args.embed, sidecar, offset_val, args.debug, recursive).await;
        } else {
            eprintln!("Error: Multiple audio files found. Use --fetch --all, provide FILE, or pass a query.");
        }
        return;
    }

    // No fetch flags — this is the local JSON conversion path
    if args.all {
        let json_files = collect_json_files(Path::new("."), recursive);
        for path in json_files {
            if let Ok(content) = fs::read_to_string(&path) {
                let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                let parent_dir = path.parent().unwrap_or_else(|| Path::new("."));
                convert_and_embed(&content, &stem, force_lrc, args.embed, sidecar, parent_dir, recursive, args.debug, offset_val);
            }
        }
        return;
    }

    if let Some(file_path) = args.file {
        if let Ok(content) = fs::read_to_string(&file_path) {
            let stem = file_path.file_stem().unwrap_or_default().to_string_lossy();
            let parent_dir = file_path.parent().unwrap_or_else(|| Path::new("."));
            convert_and_embed(&content, &stem, force_lrc, args.embed, sidecar, parent_dir, recursive, args.debug, offset_val);
        } else {
            eprintln!("Error: File not found or unreadable: {}", file_path.display());
        }
    }
}
