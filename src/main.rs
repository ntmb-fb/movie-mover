use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, SystemTime};
use std::{env, fs, io, thread};

const VIDEO: &[&str] = &["mkv", "mp4", "avi", "m4v", "mov", "ts", "wmv"];
const SUBS: &[&str] = &["srt", "sub", "idx", "ass"];
const MUSIC: &[&str] = &[
    "flac", "mp3", "m4a", "wav", "ogg", "opus", "ape", "wma", "aiff", "dsf",
];
// Videos below this size are samples.
const BIG: u64 = 100 * 1024 * 1024;

#[derive(Default)]
struct Config {
    dst: PathBuf,
    series: PathBuf,
    token: Option<String>,
    dry_run: bool,
    log_dir: Option<PathBuf>,
}

/// UTC "YYYY-MM-DD HH:MM:SS" for seconds since 1970 (days-to-date by Howard Hinnant).
fn timestamp(secs: u64) -> String {
    let (days, rem) = (secs / 86400 + 719_468, secs % 86400);
    let (era, doe) = (days / 146_097, days % 146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    format!("{year:04}-{month:02}-{day:02} {h:02}:{m:02}:{s:02}")
}

fn unix_now() -> u64 {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH);
    now.unwrap_or_default().as_secs()
}

const LOG_FILE: &str = "movie-mover.log";

/// Drop the log lines written before `oldest` (seconds since 1970).
/// Every line starts with its timestamp.
fn prune_log(dir: &Path, oldest: u64) -> io::Result<()> {
    let file = dir.join(LOG_FILE);
    let oldest = timestamp(oldest);
    let text = fs::read_to_string(&file)?;
    let kept: String = text
        .split_inclusive('\n')
        .filter(|line| *line >= oldest.as_str())
        .collect();
    if kept.len() < text.len() {
        // Write beside it and rename: a crash never leaves half a log.
        let partial = dir.join("movie-mover.log.partial");
        fs::write(&partial, kept)?;
        fs::rename(partial, file)?;
    }
    Ok(())
}

impl Config {
    /// To stdout (the container log) and, with a log folder, to the log file.
    fn log(&self, line: String) {
        println!("{line}");
        let Some(dir) = &self.log_dir else { return };
        let now = unix_now();
        let file = dir.join(LOG_FILE);
        let written = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
            .and_then(|mut f| writeln!(f, "{} {line}", timestamp(now)));
        if let Err(e) = written {
            eprintln!("cannot write {}: {e}", file.display());
        }
    }
}

type Files = Vec<(PathBuf, fs::Metadata)>;

fn ext(p: &Path) -> String {
    p.extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase()
}

fn stem(p: &Path) -> String {
    p.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// All regular files under `p` (or `p` itself). Symlinks are not followed.
fn walk(p: &Path, out: &mut Files) -> io::Result<()> {
    let meta = fs::symlink_metadata(p)?;
    if meta.is_dir() {
        for entry in fs::read_dir(p)? {
            walk(&entry?.path(), out)?;
        }
    } else if meta.is_file() {
        out.push((p.to_path_buf(), meta));
    }
    Ok(())
}

/// Changes while a download or copy is still being written.
fn signature(files: &Files) -> (usize, u64, SystemTime) {
    let size = files.iter().map(|(_, m)| m.len()).sum();
    let newest = files.iter().filter_map(|(_, m)| m.modified().ok()).max();
    (files.len(), size, newest.unwrap_or(SystemTime::UNIX_EPOCH))
}

/// A release name split into its words.
fn words(name: &str) -> Vec<String> {
    let clean = name.replace(['.', '_', '(', ')', '[', ']'], " ");
    clean.split_whitespace().map(String::from).collect()
}

/// "S01E02", "s1e2" or "S01E02E03" -> (1, "S01E02"). Anything else is no episode.
fn episode_code(word: &str) -> Option<(u32, String)> {
    let upper = word.to_uppercase();
    let (season, episodes) = upper.strip_prefix('S')?.split_once('E')?;
    let number = |s: &str| (1..=3).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
    if !number(season) || !episodes.split('E').all(number) {
        return None;
    }
    let season: u32 = season.parse().ok()?;
    let episodes: String = episodes.split('E').map(|e| format!("E{e:0>2}")).collect();
    Some((season, format!("S{season:02}{episodes}")))
}

/// "Shogun.2024.S01E02.1080p" -> ("Shogun 2024", 1, "S01E02").
fn parse_episode(name: &str) -> Option<(String, u32, String)> {
    let words = words(name);
    let (i, (season, code)) = words
        .iter()
        .enumerate()
        .find_map(|(i, word)| Some((i, episode_code(word)?)))?;
    Some((words[..i].join(" "), season, code))
}

/// The folder in `dir` with the same name as `name`, ignoring case and punctuation.
fn existing_folder(dir: &Path, name: &str) -> Option<String> {
    let plain = |s: &str| -> String {
        let letters = s.chars().filter(|c| c.is_alphanumeric());
        letters.flat_map(char::to_lowercase).collect()
    };
    let wanted = plain(name);
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|folder| !wanted.is_empty() && plain(folder) == wanted)
}

/// "Fall.2.Deadpoint.2026.Hybrid.2160p" -> ("Fall 2 Deadpoint", "2026").
/// The year is the last 19xx/20xx token, so "1922 (2017)" keeps "1922" as title.
fn parse(name: &str) -> Option<(String, String)> {
    let tokens = words(name);
    // No future years: "Blade Runner 2049 1080p" has no release year.
    let next_year = 1971 + unix_now() / 31_556_952;
    let is_year = |t: &String| {
        t.len() == 4
            && t.parse()
                .is_ok_and(|y: u64| (1900..=next_year).contains(&y))
    };
    let i = tokens.iter().rposition(|t| is_year(t)).filter(|&i| i > 0)?;
    Some((tokens[..i].join(" "), tokens[i].to_string()))
}

/// Drop characters the shares (SMB) do not accept. This also keeps a TMDB
/// title from escaping the target folder.
fn sanitize(title: &str) -> String {
    let kept = title.replace([':', '/', '\\', '?', '*', '"', '<', '>', '|'], "");
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Letter folder: leading The/A/An is ignored, digits and X/Y/Z share a folder.
fn bucket(title: &str) -> String {
    let lower = title.to_lowercase();
    let rest = ["the ", "an ", "a "]
        .iter()
        .find_map(|article| lower.strip_prefix(article))
        .unwrap_or(&lower);
    match rest.chars().find(|c| c.is_alphanumeric()) {
        Some('x' | 'y' | 'z') => "X-Y-Z".into(),
        Some(c) if !c.is_numeric() => c.to_uppercase().to_string(),
        _ => "0-9".into(),
    }
}

/// Official title and first year of the first TMDB match. `kind` is "movie" or "tv".
fn tmdb(
    agent: &ureq::Agent,
    token: &str,
    kind: &str,
    title: &str,
    year: Option<&str>,
) -> Result<Option<(String, String)>, ureq::Error> {
    let (title_key, date_key, year_key) = match kind {
        "tv" => ("name", "first_air_date", "first_air_date_year"),
        _ => ("title", "release_date", "year"),
    };
    let mut request = agent
        .get(format!("https://api.themoviedb.org/3/search/{kind}"))
        .header("Authorization", format!("Bearer {token}"))
        .query("query", title);
    if let Some(year) = year {
        request = request.query(year_key, year);
    }
    let json: serde_json::Value = request.call()?.body_mut().read_json()?;
    let first = &json["results"][0];
    let title = sanitize(first[title_key].as_str().unwrap_or_default());
    let year = first[date_key].as_str().and_then(|d| d.get(..4));
    Ok(match year {
        Some(y) if !title.is_empty() && y.parse::<u16>().is_ok() => Some((title, y.into())),
        _ => None,
    })
}

/// Rename, or copy and delete when source and target are on different filesystems.
fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => copy_then_delete(from, to),
        result => result,
    }
}

fn copy_then_delete(from: &Path, to: &Path) -> io::Result<()> {
    // Copy under a temporary name: a half-written file never has its final name.
    let mut partial = to.as_os_str().to_owned();
    partial.push(".partial");
    fs::copy(from, &partial)?;
    fs::File::open(&partial)?.sync_all()?;
    fs::rename(&partial, to)?;
    fs::remove_file(from)
}

fn is_video(p: &Path) -> bool {
    VIDEO.contains(&ext(p).as_str())
}

fn is_sub(p: &Path) -> bool {
    SUBS.contains(&ext(p).as_str())
}

/// Folder name for a show: one that is already in the library, else TMDB's name,
/// else the name from the release.
fn show_folder(cfg: &Config, agent: &ureq::Agent, show: &str, entry_name: &str) -> String {
    // "Shogun 2024" may be filed as "Shogun.2024" or as plain "Shogun".
    let (title, year) = match parse(show) {
        Some((title, year)) => (title, Some(year)),
        None => (show.to_string(), None),
    };
    let existing = |name: &str| existing_folder(&cfg.series, name);
    if let Some(folder) = existing(show).or_else(|| existing(&title)) {
        return folder;
    }
    if let Some(token) = &cfg.token {
        match tmdb(agent, token, "tv", &title, year.as_deref()) {
            Ok(Some((name, _))) => return existing(&name).unwrap_or(name),
            Ok(None) => cfg.log(format!("{entry_name}: no TMDB match, using parsed name")),
            Err(e) => cfg.log(format!(
                "{entry_name}: TMDB failed ({e}), using parsed name"
            )),
        }
    }
    sanitize(show)
}

/// Move every episode of a download to `<Show>/Season NN/<Show> - S01E02.<ext>`.
fn process_series(
    cfg: &Config,
    agent: &ureq::Agent,
    entry: &Path,
    entry_name: &str,
    files: &Files,
) -> io::Result<String> {
    if !cfg.series.is_dir() {
        return Ok("an episode, but there is no series folder, left alone".into());
    }
    let from_entry = parse_episode(entry_name);
    let mut folders = HashMap::new();
    // (video name, episode code, target folder, target name) of each moved episode.
    let mut moved = Vec::new();
    let mut left = 0;
    let episodes = files
        .iter()
        .filter(|(p, _)| is_video(p) && !stem(p).to_lowercase().contains("sample"));
    for (video, _) in episodes {
        // The episode code comes from the file, else from the folder name. The show
        // name is the other way round: folder names keep the capitals more often.
        let own = parse_episode(&stem(video));
        let code = [&own, &from_entry].into_iter().flatten().next();
        let mut shows = [&from_entry, &own].into_iter().flatten();
        let show = shows.find_map(|(show, ..)| Some(sanitize(show)).filter(|s| !s.is_empty()));
        let (Some((_, season, code)), Some(show)) = (code, show) else {
            left += 1;
            continue;
        };
        let folder = folders
            .entry(show.clone())
            .or_insert_with(|| show_folder(cfg, agent, &show, entry_name));
        let dir = cfg
            .series
            .join(&*folder)
            .join(format!("Season {season:02}"));
        let name = format!("{folder} - {code}");
        let target = dir.join(format!("{name}.{}", ext(video)));
        if target.exists() {
            cfg.log(format!(
                "{entry_name}: {} already exists, left alone",
                target.display()
            ));
            left += 1;
        } else if cfg.dry_run {
            cfg.log(format!("{entry_name}: would move to {}", target.display()));
        } else {
            fs::create_dir_all(&dir)?;
            move_file(video, &target)?;
            cfg.log(format!("{entry_name}: moved to {}", target.display()));
            moved.push((stem(video), code.clone(), dir, name));
        }
    }
    if cfg.dry_run {
        return Ok("dry run, nothing moved".into());
    }

    for (sub, _) in files.iter().filter(|(p, _)| is_sub(p)) {
        // A subtitle belongs to the video it is named after, or to the same episode code,
        // or to the only episode of the download.
        let sub_stem = stem(sub);
        let code = parse_episode(&sub_stem).map(|(.., code)| code);
        let owner = moved
            .iter()
            .find(|(video, ..)| sub_stem.starts_with(video.as_str()))
            .or_else(|| moved.iter().find(|(_, c, ..)| Some(c) == code.as_ref()))
            .or(moved.first().filter(|_| moved.len() == 1));
        let Some((video, _, dir, name)) = owner else {
            left += 1;
            continue;
        };
        // Keep ".en" from "Show.S01E02.en"; other endings are not a language.
        let language = sub_stem.rsplit_once('.').map(|(_, last)| last);
        let suffix = match (sub_stem.strip_prefix(video.as_str()), language) {
            (Some(rest), _) => rest.to_string(),
            (None, Some(l)) if (2..=3).contains(&l.len()) && l.chars().all(char::is_alphabetic) => {
                format!(".{l}")
            }
            _ => String::new(),
        };
        let target = dir.join(format!("{name}{suffix}.{}", ext(sub)));
        if target.exists() {
            left += 1;
        } else {
            move_file(sub, &target)?;
        }
    }

    // Delete the rest (samples, thumbnails, nfo) only when nothing of value stayed behind.
    let big_left = files.iter().any(|(p, m)| m.len() >= BIG && p.exists());
    if left > 0 || big_left || moved.is_empty() {
        return Ok(format!(
            "{} episode(s) moved, source kept: {left} file(s) not placed",
            moved.len()
        ));
    }
    if entry.is_dir() {
        fs::remove_dir_all(entry)?;
    }
    Ok(format!("{} episode(s) moved", moved.len()))
}

/// Handle one entry of the source folder. Returns the line to log.
fn process(cfg: &Config, agent: &ureq::Agent, entry: &Path, files: &Files) -> io::Result<String> {
    let entry_name = if entry.is_dir() {
        entry
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    } else {
        stem(entry)
    };
    // An album can hold a video and a year in its name; it must not be taken for a movie.
    if files.iter().any(|(p, _)| MUSIC.contains(&ext(p).as_str())) {
        return Ok("music, left alone".into());
    }
    let is_episode = |name: &str| parse_episode(name).is_some();
    if is_episode(&entry_name)
        || files
            .iter()
            .any(|(p, _)| is_video(p) && is_episode(&stem(p)))
    {
        return process_series(cfg, agent, entry, &entry_name, files);
    }

    // Every big file counts, video or not: the rest of the folder is deleted later.
    let big: Vec<_> = files.iter().filter(|(_, m)| m.len() >= BIG).collect();
    let mut videos = files.iter().filter(|(p, _)| is_video(p));
    let video = match (big.as_slice(), videos.next()) {
        ([one], _) if is_video(&one.0) => &one.0,
        ([], Some(only)) if videos.next().is_none() => &only.0,
        // A season pack, a multi-part movie or no movie at all: not safe to guess.
        _ => return Ok("not exactly one movie file, left alone".into()),
    };

    let Some((mut title, mut year)) = parse(&entry_name).or_else(|| parse(&stem(video))) else {
        return Ok("no year in name, left alone".into());
    };
    title = sanitize(&title);
    if title.is_empty() {
        return Ok("no title in name, left alone".into());
    }
    if let Some(token) = &cfg.token {
        match tmdb(agent, token, "movie", &title, Some(&year)) {
            Ok(Some(found)) => (title, year) = found,
            Ok(None) => cfg.log(format!("{entry_name}: no TMDB match, using parsed name")),
            Err(e) => cfg.log(format!(
                "{entry_name}: TMDB failed ({e}), using parsed name"
            )),
        }
    }

    let name = format!("{title} ({year})");
    let dir = cfg.dst.join(bucket(&title)).join(&name);
    let target = dir.join(format!("{name}.{}", ext(video)));
    // A folder holding only .partial files is a copy that was interrupted: redo it.
    let taken = fs::read_dir(&dir)
        .is_ok_and(|entries| entries.flatten().any(|e| ext(&e.path()) != "partial"));
    if taken {
        return Ok(format!("{} already exists, left alone", dir.display()));
    }
    if cfg.dry_run {
        return Ok(format!("would move to {}", target.display()));
    }

    fs::create_dir_all(&dir)?;
    move_file(video, &target)?;
    let video_stem = stem(video);
    let mut subs_left = false;
    for (sub, _) in files.iter().filter(|(p, _)| is_sub(p)) {
        // "Movie.en.srt" keeps ".en"; an unrelated name like "English.srt" becomes the suffix.
        let sub_stem = stem(sub);
        let suffix = match sub_stem.strip_prefix(&video_stem) {
            Some(rest) => rest.to_string(),
            None => format!(".{sub_stem}"),
        };
        let sub_target = dir.join(format!("{name}{suffix}.{}", ext(sub)));
        if sub_target.exists() {
            subs_left = true;
        } else {
            move_file(sub, &sub_target)?;
        }
    }
    if subs_left {
        return Ok(format!(
            "moved to {}, source kept: subtitle name clash",
            target.display()
        ));
    }
    // What is left is samples, thumbnails and nfo files.
    if entry.is_dir() {
        fs::remove_dir_all(entry)?;
    }
    Ok(format!("moved to {}", target.display()))
}

/// What the web page and the scan loop share.
#[derive(Default)]
struct Switch {
    awake: AtomicBool,
    /// Seconds since 1970 when the service falls asleep; 0 is never.
    sleep_at: AtomicU64,
    /// Seconds between scans, changed with /interval/<seconds>.
    interval: AtomicU64,
}

/// Answer one request: /start, /stop and /interval/<seconds> set the switch,
/// every path shows the state.
fn answer(mut stream: TcpStream, switch: &Switch, log_dir: Option<&Path>) -> io::Result<()> {
    // A client that says nothing must not block the next one for long.
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    // Read the whole header: closing with unread data would reset the connection.
    let mut request = Vec::new();
    let mut chunk = [0; 1024];
    while !request.ends_with(b"\r\n\r\n") && request.len() < 16 * 1024 {
        match stream.read(&mut chunk)? {
            0 => break,
            read => request.extend_from_slice(&chunk[..read]),
        }
    }
    let request = String::from_utf8_lossy(&request);
    match request.split_whitespace().nth(1) {
        Some("/start") => switch.awake.store(true, Relaxed),
        Some("/stop") => switch.awake.store(false, Relaxed),
        Some(path) => {
            let seconds = path
                .strip_prefix("/interval/")
                .and_then(|s| s.parse::<u64>().ok());
            if let Some(seconds) = seconds.filter(|s| *s > 0) {
                switch.interval.store(seconds, Relaxed);
            }
        }
        None => {}
    }

    let minutes_left = switch
        .sleep_at
        .load(Relaxed)
        .saturating_sub(unix_now())
        .div_ceil(60);
    let state = match (switch.awake.load(Relaxed), switch.sleep_at.load(Relaxed)) {
        (false, _) => "Asleep.".to_string(),
        (true, 0) => "Awake.".to_string(),
        (true, _) => format!("Awake. Sleeps in {minutes_left} min if nothing new arrives."),
    };
    let log = log_dir.and_then(|dir| fs::read_to_string(dir.join(LOG_FILE)).ok());
    let lines: Vec<&str> = log.as_deref().unwrap_or_default().lines().collect();
    let recent = lines[lines.len().saturating_sub(20)..].join("\n");
    let recent = recent.replace('&', "&amp;").replace('<', "&lt;");
    let page = format!(
        "<!doctype html><meta name=viewport content='width=device-width'>\
         <title>movie-mover</title><h1>movie-mover</h1><p>{state} Scans every {}s (<a href=/interval/5>5</a> | \
         <a href=/interval/30>30</a> | <a href=/interval/300>300</a>).</p>\
         <p><a href=/start>Start</a> | <a href=/stop>Stop</a> | <a href=/>Refresh</a></p>\
         <pre>{recent}</pre>",
        switch.interval.load(Relaxed)
    );
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    )
}

fn main() {
    let var = |key: &str| env::var(key).ok().filter(|v| !v.is_empty());
    let src = PathBuf::from(var("SRC").unwrap_or("/Download/complete".into()));
    let cfg = Config {
        dst: var("DST").unwrap_or("/film".into()).into(),
        series: var("SERIES").unwrap_or("/serie".into()).into(),
        token: var("TMDB_TOKEN"),
        dry_run: var("DRY_RUN").is_some_and(|v| v != "0"),
        // Only when the folder is there: without a mapping the log stays on stdout.
        log_dir: Some(PathBuf::from(var("LOG_DIR").unwrap_or("/data".into())))
            .filter(|dir| dir.is_dir()),
    };
    // At least 1: a pause of 0 would scan without a break.
    let interval: u64 = var("INTERVAL")
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
        .max(1);
    let log_days: u64 = var("LOG_DAYS").and_then(|v| v.parse().ok()).unwrap_or(60);
    let port: u16 = var("PORT").and_then(|v| v.parse().ok()).unwrap_or(8767);
    let idle_minutes: u64 = var("IDLE_MINUTES")
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into();
    cfg.log(format!(
        "watching {} -> {} and {} every {interval}s, TMDB {}{}",
        src.display(),
        cfg.dst.display(),
        cfg.series.display(),
        if cfg.token.is_some() { "on" } else { "off" },
        if cfg.dry_run { ", dry run" } else { "" },
    ));

    // Awake from the start: a restart of the container counts as a start.
    let switch = Arc::new(Switch::default());
    switch.awake.store(true, Relaxed);
    switch.interval.store(interval, Relaxed);
    let listening = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(listener) => {
            let (switch, log_dir) = (switch.clone(), cfg.log_dir.clone());
            thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let _ = answer(stream, &switch, log_dir.as_deref());
                }
            });
            true
        }
        Err(e) => {
            cfg.log(format!(
                "cannot listen on port {port} ({e}), so it never sleeps"
            ));
            false
        }
    };
    // Without the page nothing could wake it again.
    let idle = if listening {
        idle_minutes.saturating_mul(60)
    } else {
        0
    };

    // Entry -> (signature, already handled). An entry is handled once its
    // signature is the same in two scans in a row.
    // shortcut: a manual copy that stalls longer than INTERVAL looks finished,
    // raise INTERVAL if a hand-copied movie ever arrives cut short.
    let mut seen = HashMap::new();
    let mut pruned_day = String::new();
    let mut was_awake = false;
    let mut last_activity = unix_now();
    loop {
        // Once a day is enough.
        let today = timestamp(unix_now())[..10].to_string();
        if let Some(dir) = cfg.log_dir.as_ref().filter(|_| today != pruned_day) {
            let oldest = unix_now().saturating_sub(log_days.saturating_mul(86400));
            match prune_log(dir, oldest) {
                // No log file yet is normal on the first start.
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    cfg.log(format!("cannot trim the log: {e}"))
                }
                _ => {}
            }
            pruned_day = today;
        }

        let awake = switch.awake.load(Relaxed);
        if awake != was_awake {
            was_awake = awake;
            last_activity = unix_now();
            // After a sleep every entry has to prove again that it is finished.
            seen.clear();
            cfg.log(if awake { "awake" } else { "asleep" }.into());
        }
        if !awake {
            thread::sleep(Duration::from_secs(1));
            continue;
        }

        // Something new, changed or handled in this scan.
        let mut active = false;
        let mut now = HashMap::new();
        match fs::read_dir(&src) {
            Err(e) => cfg.log(format!("cannot read {}: {e}", src.display())),
            Ok(entries) => {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    // _UNPACK_ and _FAILED_ are SABnzbd's unfinished and failed jobs,
                    // @eaDir and #recycle are Synology's own folders.
                    if name == "nzb"
                        || [".", "@", "#", "_UNPACK_", "_FAILED_"]
                            .iter()
                            .any(|p| name.starts_with(p))
                    {
                        continue;
                    }
                    let path = entry.path();
                    let mut files = Files::new();
                    if let Err(e) = walk(&path, &mut files) {
                        cfg.log(format!("{name}: cannot read: {e}"));
                        continue;
                    }
                    let sig = signature(&files);
                    let handled = match seen.get(&path) {
                        Some((old, handled)) if *old == sig => {
                            *handled
                                || match process(&cfg, &agent, &path, &files) {
                                    Ok(line) => {
                                        cfg.log(format!("{name}: {line}"));
                                        active = true;
                                        true
                                    }
                                    // Not marked as handled: tried again on the next scan.
                                    Err(e) => {
                                        cfg.log(format!("{name}: error: {e}"));
                                        false
                                    }
                                }
                        }
                        _ => {
                            active = true;
                            false
                        }
                    };
                    now.insert(path, (sig, handled));
                }
            }
        }
        seen = now;

        if active {
            last_activity = unix_now();
        }
        if idle > 0 {
            let sleep_at = last_activity.saturating_add(idle);
            switch.sleep_at.store(sleep_at, Relaxed);
            if unix_now() >= sleep_at {
                cfg.log(format!("nothing new for {idle_minutes} min"));
                switch.awake.store(false, Relaxed);
                continue;
            }
        }
        // In steps of a second, so a start or stop is noticed at once.
        for _ in 0..switch.interval.load(Relaxed) {
            if !switch.awake.load(Relaxed) {
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_names() {
        let p = |n| parse(n).map(|(t, y)| format!("{t}|{y}"));
        assert_eq!(
            p("Fall 2 Deadpoint 2026 Hybrid 2160p WEB-DL DV HDR DDP5 1 H265-AOC").unwrap(),
            "Fall 2 Deadpoint|2026"
        );
        assert_eq!(
            p("insidious.out.of.the.further.2026.2160p.web.h265-poke").unwrap(),
            "insidious out of the further|2026"
        );
        assert_eq!(p("1922 (2017)").unwrap(), "1922|2017");
        assert_eq!(
            p("Blade.Runner.2049.2017.1080p.BluRay").unwrap(),
            "Blade Runner 2049|2017"
        );
        assert_eq!(p("Some.Home.Video.1080p"), None);
        assert_eq!(p("2026"), None);
        assert_eq!(p("Blade.Runner.2049.1080p"), None);
    }

    #[test]
    fn sanitizes_titles() {
        assert_eq!(sanitize("X-Men: Apocalypse"), "X-Men Apocalypse");
        assert_eq!(sanitize("../../etc/passwd"), "....etcpasswd");
        assert_eq!(
            sanitize("Zack Snyder's  Justice League"),
            "Zack Snyder's Justice League"
        );
    }

    #[test]
    fn picks_buckets() {
        assert_eq!(bucket("The 12th Man"), "0-9");
        assert_eq!(bucket("28 Years Later"), "0-9");
        assert_eq!(bucket("The Avengers"), "A");
        assert_eq!(bucket("A Touch of Zen"), "T");
        assert_eq!(bucket("An Education"), "E");
        assert_eq!(bucket("Zero"), "X-Y-Z");
        assert_eq!(bucket("xXx Return of Xander Cage"), "X-Y-Z");
        assert_eq!(bucket("Them"), "T");
        assert_eq!(bucket("'71"), "0-9");
    }

    #[test]
    fn moves_a_download() {
        let root = env::temp_dir().join(format!("movie-mover-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let job = root.join("complete/The.Brink.of.War.2026.1080p.AMZN.WEB-DL-SCOPE");
        fs::create_dir_all(job.join("Subs")).unwrap();
        fs::write(
            job.join("The.Brink.of.War.2026.1080p.AMZN.WEB-DL-SCOPE.mkv"),
            "video",
        )
        .unwrap();
        fs::write(
            job.join("The.Brink.of.War.2026.1080p.AMZN.WEB-DL-SCOPE.en.srt"),
            "sub",
        )
        .unwrap();
        fs::write(job.join("Subs/Danish.srt"), "sub").unwrap();
        fs::write(job.join("thumbnail.jpg"), "junk").unwrap();

        let mut cfg = Config {
            dst: root.join("film"),
            dry_run: true,
            log_dir: None,
            ..Default::default()
        };
        let agent = ureq::Agent::new_with_defaults();
        let mut files = Files::new();
        walk(&job, &mut files).unwrap();

        let movie = root.join("film/B/The Brink of War (2026)");
        process(&cfg, &agent, &job, &files).unwrap();
        assert!(job.exists() && !movie.exists(), "dry run changes nothing");

        cfg.dry_run = false;
        process(&cfg, &agent, &job, &files).unwrap();
        assert!(movie.join("The Brink of War (2026).mkv").exists());
        assert!(movie.join("The Brink of War (2026).en.srt").exists());
        assert!(movie.join("The Brink of War (2026).Danish.srt").exists());
        assert!(!job.exists(), "source folder is removed");

        // A second download of the same movie is not overwritten or deleted.
        fs::create_dir_all(&job).unwrap();
        fs::write(job.join("again.mkv"), "other").unwrap();
        let mut files = Files::new();
        walk(&job, &mut files).unwrap();
        process(&cfg, &agent, &job, &files).unwrap();
        assert!(job.join("again.mkv").exists());
        assert_eq!(
            fs::read_to_string(movie.join("The Brink of War (2026).mkv")).unwrap(),
            "video"
        );

        fs::remove_dir_all(&root).unwrap();
    }

    fn job(root: &Path, names: &[&str]) -> (PathBuf, Files) {
        let job = root.join("complete/Some.Movie.2020.1080p");
        for name in names {
            fs::create_dir_all(job.join(name).parent().unwrap()).unwrap();
            fs::write(job.join(name), "x").unwrap();
        }
        let mut files = Files::new();
        walk(&job, &mut files).unwrap();
        (job, files)
    }

    #[test]
    fn keeps_what_it_cannot_place() {
        let root = env::temp_dir().join(format!("movie-mover-keep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let cfg = Config {
            dst: root.join("film"),
            dry_run: false,
            log_dir: None,
            ..Default::default()
        };
        let agent = ureq::Agent::new_with_defaults();
        let movie = root.join("film/S/Some Movie (2020)");

        // Two videos: nothing is touched.
        let (dir, files) = job(&root, &["cd1.mkv", "cd2.mkv"]);
        process(&cfg, &agent, &dir, &files).unwrap();
        assert!(dir.join("cd1.mkv").exists() && !movie.exists());
        fs::remove_dir_all(&dir).unwrap();

        // One movie next to another big file (sparse here): nothing is touched.
        let (dir, _) = job(&root, &["movie.mkv", "disc.iso"]);
        for name in ["movie.mkv", "disc.iso"] {
            fs::File::create(dir.join(name))
                .unwrap()
                .set_len(BIG)
                .unwrap();
        }
        let mut files = Files::new();
        walk(&dir, &mut files).unwrap();
        process(&cfg, &agent, &dir, &files).unwrap();
        assert!(dir.join("disc.iso").exists() && !movie.exists());
        fs::remove_file(dir.join("disc.iso")).unwrap();
        let mut files = Files::new();
        walk(&dir, &mut files).unwrap();
        process(&cfg, &agent, &dir, &files).unwrap();
        assert!(movie.join("Some Movie (2020).mkv").exists() && !dir.exists());
        fs::remove_dir_all(&movie).unwrap();

        // An album with a bonus video and a year in its name stays where it is.
        let (dir, files) = job(&root, &["01 - track.flac", "bonus.mp4"]);
        process(&cfg, &agent, &dir, &files).unwrap();
        assert!(dir.join("01 - track.flac").exists() && !movie.exists());
        fs::remove_dir_all(&dir).unwrap();

        // An interrupted copy is redone; clashing subtitles keep the source folder.
        fs::create_dir_all(&movie).unwrap();
        fs::write(movie.join("Some Movie (2020).mkv.partial"), "half").unwrap();
        let (dir, files) = job(&root, &["a.mkv", "eng/English.srt", "forced/English.srt"]);
        process(&cfg, &agent, &dir, &files).unwrap();
        assert!(movie.join("Some Movie (2020).mkv").exists());
        assert!(movie.join("Some Movie (2020).English.srt").exists());
        assert!(dir.exists(), "one subtitle is still in the source folder");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn copies_across_filesystems() {
        let root = env::temp_dir().join(format!("movie-mover-copy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let (from, to) = (root.join("from.mkv"), root.join("to.mkv"));
        fs::write(&from, "video").unwrap();
        copy_then_delete(&from, &to).unwrap();
        assert_eq!(fs::read_to_string(&to).unwrap(), "video");
        assert!(!from.exists() && !root.join("to.mkv.partial").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn writes_and_prunes_the_log() {
        assert_eq!(timestamp(0), "1970-01-01 00:00:00");
        assert_eq!(timestamp(1_700_000_000), "2023-11-14 22:13:20");
        assert_eq!(timestamp(1_709_164_800), "2024-02-29 00:00:00");

        let dir = env::temp_dir().join(format!("movie-mover-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let now = unix_now();
        let (old, recent) = (timestamp(now - 31 * 86400), timestamp(now - 29 * 86400));
        fs::write(dir.join(LOG_FILE), format!("{old} old\n{recent} recent\n")).unwrap();

        let cfg = Config {
            dst: dir.clone(),
            dry_run: true,
            log_dir: Some(dir.clone()),
            ..Default::default()
        };
        cfg.log("hello".into());
        prune_log(&dir, now - 30 * 86400).unwrap();
        let text = fs::read_to_string(dir.join(LOG_FILE)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(lines[0], format!("{recent} recent"));
        assert!(lines[1].ends_with(" hello"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_episode_codes() {
        let p = |n| parse_episode(n).map(|(show, season, code)| format!("{show}|{season}|{code}"));
        assert_eq!(
            p("Shogun.2024.S01E02.1080p.WEB").unwrap(),
            "Shogun 2024|1|S01E02"
        );
        assert_eq!(p("the.show.s1e2.720p").unwrap(), "the show|1|S01E02");
        assert_eq!(p("Show S02E05E06 Double").unwrap(), "Show|2|S02E05E06");
        assert_eq!(p("S03E04"), Some("|3|S03E04".into()));
        assert_eq!(p("Se7en.1995.1080p"), None);
        assert_eq!(p("Fall.2.Deadpoint.2026"), None);
    }

    #[test]
    fn files_a_season_pack() {
        let root = env::temp_dir().join(format!("movie-mover-series-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut cfg = Config {
            dst: root.join("film"),
            series: root.join("serie"),
            ..Default::default()
        };
        let agent = ureq::Agent::new_with_defaults();
        let pack = [
            "Shogun.2024.S01E01.1080p.mkv",
            "Shogun.2024.S01E01.1080p.da.srt",
            "Shogun.2024.S01E02.1080p.mkv",
            "Subs/shogun.2024.s01e02.en.srt",
            "sample-shogun.s01e01.mkv",
            "info.nfo",
        ];
        let make = |names: &[&str]| {
            let dir = root.join("complete/Shogun.2024.S01.1080p.WEB");
            for name in names {
                fs::create_dir_all(dir.join(name).parent().unwrap()).unwrap();
                fs::write(dir.join(name), "x").unwrap();
            }
            let mut files = Files::new();
            walk(&dir, &mut files).unwrap();
            (dir, files)
        };

        // Without a series folder an episode is not filed, and never as a movie.
        let (dir, files) = make(&pack);
        process(&cfg, &agent, &dir, &files).unwrap();
        assert!(dir.join(pack[0]).exists() && !root.join("film").exists());

        // The show folder that is already there is used, whatever its punctuation.
        let season = root.join("serie/Shogun.2024/Season 01");
        fs::create_dir_all(root.join("serie/Shogun.2024")).unwrap();
        cfg.dry_run = true;
        process(&cfg, &agent, &dir, &files).unwrap();
        assert!(
            dir.join(pack[0]).exists() && !season.exists(),
            "dry run changes nothing"
        );
        cfg.dry_run = false;
        process(&cfg, &agent, &dir, &files).unwrap();
        for name in ["S01E01.mkv", "S01E01.da.srt", "S01E02.mkv", "S01E02.en.srt"] {
            assert!(
                season.join(format!("Shogun.2024 - {name}")).exists(),
                "{name}"
            );
        }
        assert!(!dir.exists(), "sample and nfo go with the folder");

        // An episode that is already in the library stays in the download folder.
        let (dir, files) = make(&[
            "Shogun.2024.S01E02.1080p.mkv",
            "Shogun.2024.S01E03.1080p.mkv",
        ]);
        process(&cfg, &agent, &dir, &files).unwrap();
        assert!(season.join("Shogun.2024 - S01E03.mkv").exists());
        assert!(dir.join("Shogun.2024.S01E02.1080p.mkv").exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn page_starts_and_stops() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let switch = Arc::new(Switch::default());
        let served = switch.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = answer(stream, &served, None);
            }
        });
        let get = |path: &str| {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(stream, "GET {path} HTTP/1.1\r\n\r\n").unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).unwrap();
            reply
        };
        assert!(get("/").contains("Asleep."));
        assert!(get("/start").starts_with("HTTP/1.1 200 OK") && switch.awake.load(Relaxed));
        assert!(get("/favicon.ico").contains("Awake.") && switch.awake.load(Relaxed));
        assert!(get("/stop").contains("Asleep.") && !switch.awake.load(Relaxed));
        get("/interval/7");
        get("/interval/0");
        get("/interval/x");
        assert_eq!(switch.interval.load(Relaxed), 7);
    }
}
