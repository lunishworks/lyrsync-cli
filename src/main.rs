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

    #[arg(short, long)]
    all: bool,

    #[arg(long = "auto", alias = "auto-tag")]
    auto: bool,

    #[arg(long, conflicts_with = "elrc")]
    lrc: bool,

    #[arg(long, conflicts_with = "lrc")]
    elrc: bool,

    #[arg(short, long)]
    embed: bool,

    #[arg(short, long, num_args = 0..=1, default_missing_value = "")]
    fetch: Option<String>,

    /// Recursively scan all subdirectories for files
    #[arg(short = 'r', long)]
    recursive: bool,

    /// Strip embedded lyrics from audio files
    #[arg(long)]
    remove: bool,

    #[arg(long)]
    debug: bool,

    #[arg(long, allow_hyphen_values = true)]
    offset: Option<f64>,
}

#[derive(Deserialize, Debug)]
struct Word {
    c: String,
    o: f64,
}

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
    if args.lrc {
        LyricsMode::Lrc
    } else {
        LyricsMode::Elrc
    }
}

fn format_time(seconds: f64) -> String {
    // LRC format strictly expects [mm:ss.xx]. 
    // Converting everything to total hundredths of a second first prevents weird floating point rounding bugs.
    let safe_seconds = seconds.max(0.0);
    let total_hundredths = (safe_seconds * 100.0).round() as u64;
    
    let mins = total_hundredths / 6000;
    let secs = (total_hundredths / 100) % 60;
    let hundredths = total_hundredths % 100;
    
    format!("{:02}:{:02}.{:02}", mins, secs, hundredths)
}

fn normalize_string(input: &str) -> String {
    // Brute force weird accents and special characters down to plain ASCII.
    // If we don't do this, our sloppy local filename matching will completely fail on foreign song titles.
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
    path.is_file() && path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| SUPPORTED_AUDIO_EXTS.contains(&ext.to_lowercase().as_str()))
}

fn strip_bracketed_sections(input: &str) -> String {
    // Strips out junk like "(Acoustic Version)" or "[Remastered 2011]" 
    // If we leave these in, the Musixmatch API search will usually return zero results.
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

fn clean_filename_component(input: &str) -> String {
    strip_bracketed_sections(input)
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_string()
}

fn parse_artist_title_query(input: &str) -> Option<(String, String)> {
    let trimmed = input.trim();
    // Try the most common dash types until one of them successfully splits the string
    let (raw_artist, raw_title) = [" - ", " – ", " — ", "-"]
        .iter()
        .find_map(|sep| trimmed.split_once(sep))?;

    let artist = clean_filename_component(raw_artist);
    let title = clean_filename_component(raw_title);
    
    if artist.is_empty() || title.is_empty() {
        return None;
    }
    Some((artist, title))
}

fn process_json_to_elrc(content: &str, force_lrc: bool, offset_val: f64, debug: bool, log: &mut String) -> Option<String> {
    let Ok(lines) = serde_json::from_str::<Vec<Line>>(content) else {
        if debug { writeln!(log, "[DEBUG] Serde parsing failed.").unwrap(); }
        return None;
    };

    let mut output = String::new();
    for line in lines {
        let shifted_ts = line.ts + offset_val;
        output.push_str(&format!("[{}]", format_time(shifted_ts)));

        if !force_lrc && line.l.is_some() {
            for word in line.l.as_ref().unwrap() {
                if word.c.trim().is_empty() {
                    output.push_str(&word.c);
                } else {
                    output.push_str(&format!("<{}>{}", format_time(shifted_ts + word.o), word.c));
                }
            }
            output.push_str(&format!("<{}>\n", format_time(line.te + offset_val)));
        } else {
            output.push_str(&format!("{}\n", line.x));
        }
    }

    (!output.trim().is_empty()).then_some(output)
}

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

async fn fetch_richsync_converted(
    musixmatch: &Musixmatch, artist: &str, title: &str, force_lrc: bool, offset_val: f64, debug: bool, log: &mut String
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

async fn fetch_standard_lrc(musixmatch: &Musixmatch, artist: &str, title: &str, debug: bool, log: &mut String) -> Option<String> {
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

    // Nuke existing lyric tags first. Lofty can act weird and write duplicate tags if we don't wipe the slate clean.
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

// Completely purges lyrics from an audio file without touching the actual audio data
fn remove_lyrics_from_audio(audio_path: &Path, debug: bool, log: &mut String) {
    let Ok(mut tagged_file) = Probe::open(audio_path).and_then(|p| p.read()) else {
        writeln!(log, "  -> Failed to open audio file {}", audio_path.display()).unwrap();
        return;
    };

    let mut removed = false;
    
    // Lofty is strict and won't let us just loop over all tags mutably.
    // We specifically ask for the exact tag types we want to edit.
    for tag_type in [TagType::Id3v2, TagType::VorbisComments, TagType::Mp4Ilst] {
        if let Some(tag) = tagged_file.tag_mut(tag_type) {
            // .remove_key() returns () in this crate version so we call it on its own line
            tag.remove_key(&ItemKey::Lyrics);
            if debug { writeln!(log, "[DEBUG] Stripped lyrics key from {:?}", tag_type).unwrap(); }
            removed = true;
        }
    }

    // Even if it just cleaned an empty tag we still save the file to be safe
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

async fn fetch_lyrics_auto(
    musixmatch: &Musixmatch, artist: &str, title: &str, lyrics_mode: LyricsMode, offset_val: f64, debug: bool, log: &mut String
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

fn parse_filename_for_tags(stem: &str, debug: bool, log: &mut String) -> Option<(String, String)> {
    let without_prefix = stem.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == '-' || c == '_' || c.is_whitespace());
    let (raw_artist, raw_title) = [" - ", " – ", " — ", "-"].iter().find_map(|sep| without_prefix.split_once(sep))?;

    let artist = clean_filename_component(raw_artist);
    let title = clean_filename_component(raw_title);

    if artist.is_empty() || title.is_empty() {
        if debug { writeln!(log, "[DEBUG] Filename '{}' produced empty tags after cleanup.", stem).unwrap(); }
        return None;
    }

    Some((artist, title))
}

fn get_artist_and_title(audio_path: &Path, debug: bool, log: &mut String) -> Option<(String, String)> {
    // Attempt to pull real metadata tags first
    if let Ok(tagged_file) = Probe::open(audio_path).and_then(|p| p.read()) {
        if let Some(tag) = tagged_file.primary_tag().or_else(|| tagged_file.first_tag()) {
            if let (Some(a), Some(t)) = (tag.artist().map(|s| s.into_owned()), tag.title().map(|s| s.into_owned())) {
                return Some((a, t));
            }
        }
    }

    // Fall back to blindly splitting the filename if metadata is missing
    let stem = audio_path.file_stem().unwrap_or_default().to_string_lossy();
    if debug { writeln!(log, "[DEBUG] Metadata missing, falling back to filename: '{}'", stem).unwrap(); }
    parse_filename_for_tags(&stem, debug, log)
}

// We use a classic vector stack loop here to crawl directories.
// This prevents us from blowing up the call stack with actual recursion, 
// and saves us from needing to add the heavy `walkdir` crate just for a simple folder scan.
fn collect_files<F>(dir: &Path, recursive: bool, filter: F) -> Vec<PathBuf> 
where F: Fn(&Path) -> bool 
{
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];

    while let Some(current_dir) = dirs.pop() {
        if let Ok(entries) = fs::read_dir(&current_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if recursive {
                        dirs.push(path);
                    }
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

fn find_matching_audio_file(base_name: &str, search_dir: &Path, recursive: bool, debug: bool, log: &mut String) -> Option<PathBuf> {
    let normalized_base = normalize_string(base_name);
    if normalized_base.is_empty() { return None; }

    collect_audio_files(search_dir, recursive).into_iter().find(|path| {
        let normalized_audio = normalize_string(&path.file_stem().unwrap_or_default().to_string_lossy());
        let matches = normalized_audio.contains(&normalized_base) || normalized_base.contains(&normalized_audio);
        if matches && debug { writeln!(log, "[DEBUG] Matched '{}' for target '{}'", path.display(), base_name).unwrap(); }
        matches
    })
}

fn write_lyrics_outputs(
    lyrics: &str, base_name: &str, embed: bool, current_dir: &Path, explicit_audio_path: Option<&Path>, recursive: bool, debug: bool, log: &mut String
) {
    let lrc_path = current_dir.join(format!("{}.lrc", base_name));
    if let Err(e) = fs::write(&lrc_path, lyrics) {
        writeln!(log, "  -> Failed to write LRC file: {}", e).unwrap();
    } else {
        writeln!(log, "  -> Saved physical file to {}", lrc_path.display()).unwrap();
    }

    if !embed { return; }

    let target_audio = explicit_audio_path
        .map(Path::to_path_buf)
        .or_else(|| find_matching_audio_file(base_name, current_dir, recursive, debug, log));

    if let Some(audio_path) = target_audio {
        embed_into_audio(&audio_path, lyrics, debug, log);
    } else {
        writeln!(log, "  -> No matching audio file found to embed into.").unwrap();
    }
}

fn convert_and_embed(
    content: &str, base_name: &str, force_lrc: bool, embed: bool, current_dir: &Path, recursive: bool, debug: bool, offset_val: f64,
) {
    let mut log = String::new();
    writeln!(&mut log, "--------------------------------------------------").unwrap();
    writeln!(&mut log, "Converting JSON: {}", base_name).unwrap();
    
    if let Some(output) = process_json_to_elrc(content, force_lrc, offset_val, debug, &mut log) {
        write_lyrics_outputs(&output, base_name, embed, current_dir, None, recursive, debug, &mut log);
    }
    
    print!("{}", log); // Atomic print to avoid terminal spaghetti
}

async fn remove_for_audio_file(audio_path: PathBuf, debug: bool) {
    let mut log = String::new();
    writeln!(&mut log, "--------------------------------------------------").unwrap();
    writeln!(&mut log, "Processing Audio File: {}", audio_path.display()).unwrap();

    remove_lyrics_from_audio(&audio_path, debug, &mut log);
    
    print!("{}", log); // Atomic print
}

async fn remove_for_audio_directory(dir: &Path, recursive: bool, debug: bool) {
    let audio_files = collect_audio_files(dir, recursive);
    if audio_files.is_empty() {
        eprintln!("Error: No supported audio files found in {}.", dir.display());
        return;
    }

    let mut handles = Vec::new();
    // 50 is a safe limit for local file editing. If we uncap this, 
    // the OS will eventually throw a "Too many open files" error on massive directories.
    let semaphore = Arc::new(Semaphore::new(50));

    for path in audio_files {
        let sem_clone = Arc::clone(&semaphore);
        handles.push(tokio::spawn(async move {
            let _permit = sem_clone.acquire().await.unwrap();
            remove_for_audio_file(path, debug).await;
        }));
    }

    for handle in handles {
        let _ = handle.await;
    }
}

async fn fetch_for_audio_file(
    musixmatch: Arc<Musixmatch>, audio_path: PathBuf, lyrics_mode: LyricsMode, embed: bool, offset_val: f64, debug: bool, recursive: bool
) {
    // Build the log in memory first so thread outputs don't turn into a scrambled mess in the terminal
    let mut log = String::new();
    writeln!(&mut log, "--------------------------------------------------").unwrap();
    writeln!(&mut log, "Processing Audio File: {}", audio_path.display()).unwrap();

    let Some((artist, title)) = get_artist_and_title(&audio_path, debug, &mut log) else {
        writeln!(&mut log, "  -> Could not determine artist/title for {}", audio_path.display()).unwrap();
        print!("{}", log); 
        return;
    };

    let Some(lyrics_text) = fetch_lyrics_auto(&musixmatch, &artist, &title, lyrics_mode, offset_val, debug, &mut log).await else {
        print!("{}", log);
        return;
    };

    let stem = audio_path.file_stem().unwrap_or_default().to_string_lossy();
    let output_dir = audio_path.parent().unwrap_or_else(|| Path::new("."));
    write_lyrics_outputs(&lyrics_text, &stem, embed, output_dir, Some(&audio_path), recursive, debug, &mut log);
    
    print!("{}", log);
}

async fn fetch_for_audio_directory(
    musixmatch: Arc<Musixmatch>, dir: &Path, lyrics_mode: LyricsMode, embed: bool, offset_val: f64, debug: bool, recursive: bool
) {
    let audio_files = collect_audio_files(dir, recursive);
    if audio_files.is_empty() {
        eprintln!("Error: No supported audio files found in {}.", dir.display());
        return;
    }

    // Semaphore limits how many tasks can hit the Musixmatch API at the exact same time.
    // 5 is a very safe limit to prevent getting temporarily IP banned for spamming requests.
    let semaphore = Arc::new(Semaphore::new(5));
    let mut handles = Vec::new();

    for path in audio_files {
        let mx_clone = Arc::clone(&musixmatch);
        let sem_clone = Arc::clone(&semaphore);
        
        handles.push(tokio::spawn(async move {
            let _permit = sem_clone.acquire().await.unwrap(); 
            fetch_for_audio_file(mx_clone, path, lyrics_mode, embed, offset_val, debug, recursive).await;
        }));
    }

    for handle in handles {
        let _ = handle.await;
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let offset_val = args.offset.unwrap_or(0.0);
    let lyrics_mode = selected_lyrics_mode(&args);
    let force_lrc = matches!(lyrics_mode, LyricsMode::Lrc);
    let recursive = args.recursive;

    // Intercept everything and run the purge pipeline if --remove is passed
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
        fetch_for_audio_directory(musixmatch, Path::new("."), lyrics_mode, args.embed, offset_val, args.debug, recursive).await;
        return;
    }

    if let Some(fetch_arg) = args.fetch.as_deref() {
        let Some(musixmatch) = musixmatch else { return; };

        if args.all {
            println!("Starting fetch pipeline for all audio files...");
            fetch_for_audio_directory(musixmatch, Path::new("."), lyrics_mode, args.embed, offset_val, args.debug, recursive).await;
            return;
        }

        let query = fetch_arg.trim();
        if !query.is_empty() {
            let mut log = String::new();
            let Some((artist, title)) = parse_artist_title_query(query) else {
                eprintln!("Error: --fetch query must be in \"Artist - Title\" format.");
                return;
            };

            if let Some(lyrics) = fetch_lyrics_auto(&musixmatch, &artist, &title, lyrics_mode, offset_val, args.debug, &mut log).await {
                let clean_title = clean_filename_component(&title);
                let base_name = if clean_title.is_empty() { &title } else { &clean_title };
                write_lyrics_outputs(&lyrics, base_name, args.embed, Path::new("."), None, recursive, args.debug, &mut log);
            }
            print!("{}", log);
            return;
        }

        if let Some(file_path) = args.file {
            if !file_path.exists() || !is_supported_audio_file(&file_path) {
                eprintln!("Error: FILE must be a valid audio file when used with --fetch.");
                return;
            }
            fetch_for_audio_file(musixmatch, file_path, lyrics_mode, args.embed, offset_val, args.debug, recursive).await;
            return;
        }

        let audio_files = collect_audio_files(Path::new("."), recursive);
        if audio_files.is_empty() {
            eprintln!("Error: No supported audio files found. Use --fetch \"Artist - Title\" or --fetch --all.");
        } else if audio_files.len() == 1 {
            println!("No fetch query provided; using the only audio file in current directory.");
            fetch_for_audio_file(musixmatch, audio_files[0].clone(), lyrics_mode, args.embed, offset_val, args.debug, recursive).await;
        } else {
            eprintln!("Error: Multiple audio files found. Use --fetch --all, provide FILE, or pass a query.");
        }
        return;
    }

    if args.all {
        let json_files = collect_json_files(Path::new("."), recursive);
        for path in json_files {
            if let Ok(content) = fs::read_to_string(&path) {
                let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                let parent_dir = path.parent().unwrap_or_else(|| Path::new("."));
                convert_and_embed(&content, &stem, force_lrc, args.embed, parent_dir, recursive, args.debug, offset_val);
            }
        }
        return;
    }

    if let Some(file_path) = args.file {
        if let Ok(content) = fs::read_to_string(&file_path) {
            let stem = file_path.file_stem().unwrap_or_default().to_string_lossy();
            let parent_dir = file_path.parent().unwrap_or_else(|| Path::new("."));
            convert_and_embed(&content, &stem, force_lrc, args.embed, parent_dir, recursive, args.debug, offset_val);
        } else {
            eprintln!("Error: File not found or unreadable: {}", file_path.display());
        }
    }
}