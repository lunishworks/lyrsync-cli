use clap::Parser;
use serde::Deserialize;
use std::fs;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// Universal audio tagging dependencies
use lofty::config::WriteOptions;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, ItemValue, Tag, TagItem, TagType};

const SUPPORTED_AUDIO_EXTS: [&str; 6] = ["flac", "mp3", "opus", "m4a", "wav", "ogg"];

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LyricsMode {
    Elrc,
    Lrc,
}

/// Convert and Fetch Musixmatch Lyrics directly into your local audio files
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Input file path for direct JSON conversion, or single-file fetch when used with --fetch
    #[arg(name = "FILE")]
    file: Option<PathBuf>,

    /// Process all files in current directory (JSON conversion by default, audio fetch with --fetch)
    #[arg(short, long)]
    all: bool,

    /// Automatic audio pipeline: scan songs, fetch lyrics, write sidecar, optional embed
    #[arg(long = "auto", alias = "auto-tag")]
    auto: bool,

    /// Force regular line-synced LRC output
    #[arg(long, conflicts_with = "elrc")]
    lrc: bool,

    /// Force enhanced word-synced eLRC output (default mode)
    #[arg(long, conflicts_with = "lrc")]
    elrc: bool,

    /// Embed lyrics into audio metadata
    #[arg(short, long)]
    embed: bool,

    /// Fetch lyrics via Musixmatch. Optional query format: "Artist - Title"
    #[arg(short, long, num_args = 0..=1, default_missing_value = "")]
    fetch: Option<String>,

    /// Enable debug logging
    #[arg(long)]
    debug: bool,

    /// Shift all generated timestamps by N seconds (e.g. -1.5, 2.0)
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
    } else if args.elrc {
        LyricsMode::Elrc
    } else {
        LyricsMode::Elrc
    }
}

fn format_time(seconds: f64) -> String {
    let safe_seconds = if seconds < 0.0 { 0.0 } else { seconds };
    let total_hundredths = (safe_seconds * 100.0).round() as u64;
    let mins = total_hundredths / 6000;
    let secs = (total_hundredths / 100) % 60;
    let hundredths = total_hundredths % 100;
    format!("{:02}:{:02}.{:02}", mins, secs, hundredths)
}

fn normalize_string(input: &str) -> String {
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

fn is_supported_audio_extension(ext: &str) -> bool {
    SUPPORTED_AUDIO_EXTS.contains(&ext)
}

fn is_supported_audio_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    is_supported_audio_extension(&ext)
}

fn strip_bracketed_sections(input: &str) -> String {
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
    let stripped = strip_bracketed_sections(input).replace('_', " ");
    let collapsed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace())
        .to_string()
}

fn parse_artist_title_query(input: &str) -> Option<(String, String)> {
    let trimmed = input.trim();
    let split = [" - ", " – ", " — ", "-"]
        .iter()
        .find_map(|separator| trimmed.split_once(separator));

    let (raw_artist, raw_title) = split?;
    let artist = clean_filename_component(raw_artist);
    let title = clean_filename_component(raw_title);
    if artist.is_empty() || title.is_empty() {
        return None;
    }
    Some((artist, title))
}

fn process_json_to_elrc(
    content: &str,
    force_lrc: bool,
    offset_val: f64,
    debug: bool,
) -> Option<String> {
    let lines: Vec<Line> = match serde_json::from_str::<Vec<Line>>(content) {
        Ok(l) => l,
        Err(e) => {
            if debug {
                println!("[DEBUG] Serde parsing error: {}", e);
            }
            return None;
        }
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
            output.push_str(&format!("<{}>", format_time(line.te + offset_val)));
        } else {
            output.push_str(&line.x);
        }
        output.push('\n');
    }

    if output.trim().is_empty() {
        None
    } else {
        Some(output)
    }
}

fn extract_first_json_array(content: &str) -> Option<&str> {
    for (start_idx, ch) in content.char_indices() {
        if ch != '[' {
            continue;
        }

        let mut depth = 0usize;
        let mut end_idx = None;

        for (rel_idx, inner_ch) in content[start_idx..].char_indices() {
            match inner_ch {
                '[' => depth += 1,
                ']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end_idx = Some(start_idx + rel_idx);
                        break;
                    }
                }
                _ => {}
            }
        }

        let Some(end_idx) = end_idx else {
            continue;
        };

        let candidate = &content[start_idx..=end_idx];
        let mut chars = candidate.chars();
        if chars.next() != Some('[') {
            continue;
        }
        let next_non_ws = chars.find(|c| !c.is_whitespace());
        if matches!(next_non_ws, Some('{') | Some('[')) {
            return Some(candidate);
        }
    }

    None
}

fn run_musixmatch_cli(args: &[&str], debug: bool) -> Option<String> {
    if debug {
        println!("[DEBUG] Running musixmatch-cli {}", args.join(" "));
    }

    let mut cmd = Command::new("musixmatch-cli");
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let output = match cmd.spawn().and_then(|child| child.wait_with_output()) {
        Ok(out) => out,
        Err(e) => {
            eprintln!("  -> Failed to execute musixmatch-cli: {}", e);
            return None;
        }
    };

    if !output.status.success() {
        if debug {
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!(
                "[DEBUG] musixmatch-cli exited with {}: {}",
                output.status,
                stderr.trim()
            );
        }
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn fetch_richsync_converted(
    artist: &str,
    title: &str,
    force_lrc: bool,
    offset_val: f64,
    debug: bool,
) -> Option<String> {
    let stdout_str = run_musixmatch_cli(&["richsync", "--name", title, "--artist", artist], debug)?;
    let json_content = extract_first_json_array(&stdout_str)?;
    process_json_to_elrc(json_content, force_lrc, offset_val, debug)
}

fn fetch_standard_lrc(artist: &str, title: &str, debug: bool) -> Option<String> {
    let stdout_str = run_musixmatch_cli(
        &[
            "subtitles",
            "--name",
            title,
            "--artist",
            artist,
            "--format",
            "lrc",
        ],
        debug,
    )?;

    let trimmed = stdout_str.trim();
    if trimmed.contains('[') && trimmed.contains(']') {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn preferred_lyrics_tag_type(file_type: FileType) -> TagType {
    match file_type {
        FileType::Mpeg => TagType::Id3v2,
        FileType::Flac | FileType::Opus | FileType::Vorbis | FileType::Speex => {
            TagType::VorbisComments
        }
        FileType::Mp4 => TagType::Mp4Ilst,
        _ => file_type.primary_tag_type(),
    }
}

fn embed_into_audio(audio_path: &Path, lyrics: &str, debug: bool) {
    if debug {
        println!(
            "[DEBUG] Attempting to embed into audio file: {}",
            audio_path.display()
        );
    }

    let mut tagged_file = match Probe::open(audio_path).and_then(|p| p.read()) {
        Ok(tf) => tf,
        Err(e) => {
            eprintln!(
                "  -> Failed to open audio file {}: {}",
                audio_path.display(),
                e
            );
            return;
        }
    };

    let file_type = tagged_file.file_type();
    let tag_type = preferred_lyrics_tag_type(file_type);

    if !tagged_file.supports_tag_type(tag_type) {
        eprintln!(
            "  -> {} does not support {:?} metadata tags for lyrics.",
            audio_path.display(),
            tag_type
        );
        return;
    }

    for existing_tag_type in [TagType::Id3v2, TagType::VorbisComments, TagType::Mp4Ilst] {
        if let Some(existing_tag) = tagged_file.tag_mut(existing_tag_type) {
            existing_tag.remove_key(&ItemKey::Lyrics);
        }
    }

    if tagged_file.tag(tag_type).is_none() {
        tagged_file.insert_tag(Tag::new(tag_type));
    }

    let Some(tag) = tagged_file.tag_mut(tag_type) else {
        eprintln!(
            "  -> Failed to access {:?} tag in {}.",
            tag_type,
            audio_path.display()
        );
        return;
    };

    tag.insert(TagItem::new(
        ItemKey::Lyrics,
        ItemValue::Text(lyrics.to_string()),
    ));

    if debug {
        println!(
            "[DEBUG] Embedded lyrics into {:?} using {:?}",
            file_type, tag_type
        );
    }

    if let Err(e) = tagged_file.save_to_path(audio_path, WriteOptions::default()) {
        eprintln!(
            "  -> Failed to save metadata to {}: {}",
            audio_path.display(),
            e
        );
        eprintln!("     Make sure the file isn't open in another program!");
    } else {
        let ext = audio_path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_uppercase();
        println!("  -> Successfully embedded lyrics into {} metadata!", ext);
    }
}

fn fetch_lyrics_auto(
    artist: &str,
    title: &str,
    lyrics_mode: LyricsMode,
    offset_val: f64,
    debug: bool,
) -> Option<String> {
    println!("  -> Searching Musixmatch for: {} - {}", artist, title);

    match lyrics_mode {
        LyricsMode::Elrc => {
            if let Some(elrc) = fetch_richsync_converted(artist, title, false, offset_val, debug) {
                println!("  -> [SUCCESS] Word-by-word eLRC generated!");
                return Some(elrc);
            }

            println!("  -> Richsync unavailable. Falling back to standard line-synced LRC...");
            if let Some(lrc) = fetch_standard_lrc(artist, title, debug) {
                println!("  -> [SUCCESS] Standard LRC found!");
                return Some(lrc);
            }
        }
        LyricsMode::Lrc => {
            if let Some(lrc) = fetch_standard_lrc(artist, title, debug) {
                println!("  -> [SUCCESS] Standard LRC generated!");
                return Some(lrc);
            }

            println!("  -> Standard subtitles unavailable. Falling back to richsync conversion...");
            if let Some(lrc_from_richsync) =
                fetch_richsync_converted(artist, title, true, offset_val, debug)
            {
                println!("  -> [SUCCESS] LRC generated from richsync data!");
                return Some(lrc_from_richsync);
            }
        }
    }

    println!("  -> [FAILED] No lyrics found.");
    None
}

fn parse_filename_for_tags(stem: &str, debug: bool) -> Option<(String, String)> {
    let without_prefix = stem.trim_start_matches(|c: char| {
        c.is_ascii_digit() || c == '.' || c == '-' || c == '_' || c.is_whitespace()
    });

    let split = [" - ", " – ", " — ", "-"]
        .iter()
        .find_map(|separator| without_prefix.split_once(separator));

    let Some((raw_artist, raw_title)) = split else {
        if debug {
            println!(
                "[DEBUG] Filename '{}' does not match 'Artist - Title' format. Skipping.",
                stem
            );
        }
        return None;
    };

    let artist = clean_filename_component(raw_artist);
    let title = clean_filename_component(raw_title);

    if artist.is_empty() || title.is_empty() {
        if debug {
            println!(
                "[DEBUG] Filename '{}' produced empty artist/title after cleanup. Skipping.",
                stem
            );
        }
        return None;
    }

    if debug {
        println!(
            "[DEBUG] Filename parsed -> Artist: '{}', Title: '{}'",
            artist, title
        );
    }

    Some((artist, title))
}

/// Attempts to read Artist/Title from metadata first, falls back to filename
fn get_artist_and_title(audio_path: &Path, debug: bool) -> Option<(String, String)> {
    if let Ok(tagged_file) = Probe::open(audio_path).and_then(|p| p.read()) {
        if let Some(tag) = tagged_file
            .primary_tag()
            .or_else(|| tagged_file.first_tag())
        {
            let artist = tag.artist().map(|s| s.into_owned());
            let title = tag.title().map(|s| s.into_owned());
            if let (Some(a), Some(t)) = (artist, title) {
                if debug {
                    println!(
                        "[DEBUG] Extracted from metadata -> Artist: '{}', Title: '{}'",
                        a, t
                    );
                }
                return Some((a, t));
            }
        }
    }

    let stem = audio_path.file_stem().unwrap_or_default().to_string_lossy();
    if debug {
        println!(
            "[DEBUG] Metadata incomplete, falling back to filename parsing for: '{}'",
            stem
        );
    }
    parse_filename_for_tags(&stem, debug)
}

fn collect_audio_files(dir: &Path) -> Vec<PathBuf> {
    let mut audio_files = Vec::new();

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if is_supported_audio_file(&path) {
                audio_files.push(path);
            }
        }
    }

    audio_files.sort();
    audio_files
}

fn find_matching_audio_file(base_name: &str, search_dir: &Path, debug: bool) -> Option<PathBuf> {
    let normalized_base = normalize_string(base_name);
    if normalized_base.is_empty() {
        return None;
    }

    for path in collect_audio_files(search_dir) {
        let normalized_audio =
            normalize_string(&path.file_stem().unwrap_or_default().to_string_lossy());
        if normalized_audio.contains(&normalized_base)
            || normalized_base.contains(&normalized_audio)
        {
            if debug {
                println!(
                    "[DEBUG] Matched '{}' as target for '{}'",
                    path.display(),
                    base_name
                );
            }
            return Some(path);
        }
    }

    None
}

fn write_lyrics_outputs(
    lyrics: &str,
    base_name: &str,
    embed: bool,
    current_dir: &Path,
    explicit_audio_path: Option<&Path>,
    debug: bool,
) {
    let lrc_path = current_dir.join(format!("{}.lrc", base_name));
    if let Err(e) = fs::write(&lrc_path, lyrics) {
        eprintln!("  -> Failed to write physical LRC file: {}", e);
    } else {
        println!("  -> Saved physical file to {}", lrc_path.display());
    }

    if !embed {
        return;
    }

    let target_audio = explicit_audio_path
        .map(Path::to_path_buf)
        .or_else(|| find_matching_audio_file(base_name, current_dir, debug));

    if let Some(audio_path) = target_audio {
        embed_into_audio(&audio_path, lyrics, debug);
    } else {
        println!("  -> No matching audio file found in directory to embed into.");
    }
}

fn convert_and_embed(
    content: &str,
    base_name: &str,
    force_lrc: bool,
    embed: bool,
    current_dir: &Path,
    debug: bool,
    offset_val: f64,
) {
    if let Some(output) = process_json_to_elrc(content, force_lrc, offset_val, debug) {
        write_lyrics_outputs(&output, base_name, embed, current_dir, None, debug);
    }
}

fn fetch_for_audio_file(
    audio_path: &Path,
    lyrics_mode: LyricsMode,
    embed: bool,
    offset_val: f64,
    debug: bool,
) -> bool {
    println!("--------------------------------------------------");
    println!("Processing Audio File: {}", audio_path.display());

    let Some((artist, title)) = get_artist_and_title(audio_path, debug) else {
        eprintln!(
            "  -> Could not determine artist/title for {}",
            audio_path.display()
        );
        return false;
    };

    let Some(lyrics_text) = fetch_lyrics_auto(&artist, &title, lyrics_mode, offset_val, debug)
    else {
        return false;
    };

    let stem = audio_path.file_stem().unwrap_or_default().to_string_lossy();
    let output_dir = audio_path.parent().unwrap_or_else(|| Path::new("."));
    write_lyrics_outputs(
        &lyrics_text,
        &stem,
        embed,
        output_dir,
        Some(audio_path),
        debug,
    );
    true
}

fn fetch_for_audio_directory(
    dir: &Path,
    lyrics_mode: LyricsMode,
    embed: bool,
    offset_val: f64,
    debug: bool,
) {
    let audio_files = collect_audio_files(dir);
    if audio_files.is_empty() {
        eprintln!(
            "Error: No supported audio files found in {}.",
            dir.display()
        );
        return;
    }

    for path in audio_files {
        fetch_for_audio_file(&path, lyrics_mode, embed, offset_val, debug);
    }
}

fn main() {
    let args = Args::parse();
    let debug = args.debug;
    let offset_val = args.offset.unwrap_or(0.0);
    let lyrics_mode = selected_lyrics_mode(&args);
    let force_lrc = matches!(lyrics_mode, LyricsMode::Lrc);

    if args.auto {
        println!("Starting automatic audio fetch pipeline...");
        fetch_for_audio_directory(Path::new("."), lyrics_mode, args.embed, offset_val, debug);
        return;
    }

    if let Some(fetch_arg) = args.fetch.as_deref() {
        if args.all {
            println!("Starting fetch pipeline for all audio files...");
            fetch_for_audio_directory(Path::new("."), lyrics_mode, args.embed, offset_val, debug);
            return;
        }

        let query = fetch_arg.trim();
        if !query.is_empty() {
            let Some((artist, title)) = parse_artist_title_query(query) else {
                return eprintln!("Error: --fetch query must be in \"Artist - Title\" format.");
            };

            if let Some(lyrics) = fetch_lyrics_auto(&artist, &title, lyrics_mode, offset_val, debug)
            {
                let clean_title = clean_filename_component(&title);
                let base_name = if clean_title.is_empty() {
                    title
                } else {
                    clean_title
                };
                write_lyrics_outputs(&lyrics, &base_name, args.embed, Path::new("."), None, debug);
            }
            return;
        }

        if let Some(file_path) = args.file.as_deref() {
            if !file_path.exists() {
                return eprintln!("Error: File not found: {}", file_path.display());
            }
            if !is_supported_audio_file(file_path) {
                return eprintln!(
                    "Error: FILE must be a supported audio file (.flac, .mp3, .opus, .m4a, .wav, .ogg) when used with --fetch."
                );
            }

            fetch_for_audio_file(file_path, lyrics_mode, args.embed, offset_val, debug);
            return;
        }

        let audio_files = collect_audio_files(Path::new("."));
        match audio_files.len() {
            0 => eprintln!(
                "Error: No supported audio files found. Use --fetch \"Artist - Title\" or --fetch --all."
            ),
            1 => {
                println!(
                    "No fetch query provided; using the only audio file in current directory."
                );
                fetch_for_audio_file(&audio_files[0], lyrics_mode, args.embed, offset_val, debug);
            }
            _ => eprintln!(
                "Error: Multiple audio files found. Use --fetch --all, provide FILE, or pass a query."
            ),
        }
        return;
    }

    if args.all {
        if let Ok(entries) = fs::read_dir(".") {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("json") {
                    println!("--------------------------------------------------");
                    if let Ok(content) = fs::read_to_string(&path) {
                        convert_and_embed(
                            &content,
                            &path.file_stem().unwrap_or_default().to_string_lossy(),
                            force_lrc,
                            args.embed,
                            Path::new("."),
                            debug,
                            offset_val,
                        );
                    }
                }
            }
        }
        return;
    }

    if let Some(file_path) = args.file {
        if file_path.exists() {
            if let Ok(content) = fs::read_to_string(&file_path) {
                convert_and_embed(
                    &content,
                    &file_path.file_stem().unwrap_or_default().to_string_lossy(),
                    force_lrc,
                    args.embed,
                    file_path.parent().unwrap_or_else(|| Path::new(".")),
                    debug,
                    offset_val,
                );
            }
        } else {
            eprintln!("Error: File not found: {}", file_path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn extracts_wrapped_richsync_json_array() {
        let wrapped = "status: ok\n[{\"ts\":0.0,\"te\":1.2,\"l\":[{\"c\":\"Hi\",\"o\":0.0}],\"x\":\"Hi\"}]\ncomplete";
        let extracted = extract_first_json_array(wrapped).expect("json array should be extracted");
        let parsed: Value =
            serde_json::from_str(extracted).expect("extracted payload should be valid JSON");
        assert!(parsed.is_array());
    }

    #[test]
    fn skips_non_json_bracket_groups_when_extracting() {
        let wrapped = "[INFO] Running\n[{\"ts\":0.0,\"te\":1.2,\"l\":[],\"x\":\"Hi\"}]";
        let extracted = extract_first_json_array(wrapped).expect("json array should be extracted");
        assert_eq!(extracted, "[{\"ts\":0.0,\"te\":1.2,\"l\":[],\"x\":\"Hi\"}]");
    }

    #[test]
    fn parse_filename_handles_track_numbers_and_suffixes() {
        let parsed =
            parse_filename_for_tags("04. Duman - Hâlimiz Duman (Live) [Remastered]", false)
                .expect("filename should parse");
        assert_eq!(parsed.0, "Duman");
        assert_eq!(parsed.1, "Hâlimiz Duman");
    }

    #[test]
    fn parse_filename_accepts_compact_hyphen_separator() {
        let parsed =
            parse_filename_for_tags("01-Artist-Track_Name", false).expect("filename should parse");
        assert_eq!(parsed.0, "Artist");
        assert_eq!(parsed.1, "Track Name");
    }

    #[test]
    fn parse_fetch_query_accepts_common_separators() {
        let parsed = parse_artist_title_query("Artist — Track Name").expect("query should parse");
        assert_eq!(parsed.0, "Artist");
        assert_eq!(parsed.1, "Track Name");
    }

    #[test]
    fn parse_fetch_query_rejects_missing_separator() {
        assert!(parse_artist_title_query("Artist Track Name").is_none());
    }
}
