use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use std::{env, fs, io, thread};

const VIDEO: &[&str] = &["mkv", "mp4", "avi", "m4v", "mov", "ts", "wmv"];
const SUBS: &[&str] = &["srt", "sub", "idx", "ass"];
// Videos below this size are samples.
const BIG: u64 = 100 * 1024 * 1024;

struct Config {
    dst: PathBuf,
    token: Option<String>,
    dry_run: bool,
    log_dir: Option<PathBuf>,
}

const LOG_DAYS: u64 = 30;

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

/// Drop the log lines older than LOG_DAYS. Every line starts with its timestamp.
fn prune_log(dir: &Path, now: u64) -> io::Result<()> {
    let file = dir.join(LOG_FILE);
    let oldest = timestamp(now.saturating_sub(LOG_DAYS * 86400));
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

/// "Fall.2.Deadpoint.2026.Hybrid.2160p" -> ("Fall 2 Deadpoint", "2026").
/// The year is the last 19xx/20xx token, so "1922 (2017)" keeps "1922" as title.
fn parse(name: &str) -> Option<(String, String)> {
    let clean = name.replace(['.', '_', '(', ')', '[', ']'], " ");
    let tokens: Vec<&str> = clean.split_whitespace().collect();
    // No future years: "Blade Runner 2049 1080p" has no release year.
    let next_year = 1971 + unix_now() / 31_556_952;
    let is_year = |t: &&str| {
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

/// Official title and release year of the first TMDB match.
fn tmdb(
    agent: &ureq::Agent,
    token: &str,
    title: &str,
    year: &str,
) -> Result<Option<(String, String)>, ureq::Error> {
    let json: serde_json::Value = agent
        .get("https://api.themoviedb.org/3/search/movie")
        .header("Authorization", format!("Bearer {token}"))
        .query("query", title)
        .query("year", year)
        .call()?
        .body_mut()
        .read_json()?;
    let first = &json["results"][0];
    let title = sanitize(first["title"].as_str().unwrap_or_default());
    let year = first["release_date"].as_str().and_then(|d| d.get(..4));
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

/// Handle one entry of the source folder. Returns the line to log.
fn process(cfg: &Config, agent: &ureq::Agent, entry: &Path, files: &Files) -> io::Result<String> {
    let is_video = |p: &Path| VIDEO.contains(&ext(p).as_str());
    // Every big file counts, video or not: the rest of the folder is deleted later.
    let big: Vec<_> = files.iter().filter(|(_, m)| m.len() >= BIG).collect();
    let mut videos = files.iter().filter(|(p, _)| is_video(p));
    let video = match (big.as_slice(), videos.next()) {
        ([one], _) if is_video(&one.0) => &one.0,
        ([], Some(only)) if videos.next().is_none() => &only.0,
        // A season pack, a multi-part movie or no movie at all: not safe to guess.
        _ => return Ok("not exactly one movie file, left alone".into()),
    };

    let entry_name = if entry.is_dir() {
        entry
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    } else {
        stem(entry)
    };
    let Some((mut title, mut year)) = parse(&entry_name).or_else(|| parse(&stem(video))) else {
        return Ok("no year in name, left alone".into());
    };
    title = sanitize(&title);
    if title.is_empty() {
        return Ok("no title in name, left alone".into());
    }
    if let Some(token) = &cfg.token {
        match tmdb(agent, token, &title, &year) {
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
    for (sub, _) in files
        .iter()
        .filter(|(p, _)| SUBS.contains(&ext(p).as_str()))
    {
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

fn main() {
    let var = |key: &str| env::var(key).ok().filter(|v| !v.is_empty());
    let src = PathBuf::from(var("SRC").unwrap_or("/Download/complete".into()));
    let cfg = Config {
        dst: var("DST").unwrap_or("/film".into()).into(),
        token: var("TMDB_TOKEN"),
        dry_run: var("DRY_RUN").is_some_and(|v| v != "0"),
        // Only when the folder is there: without a mapping the log stays on stdout.
        log_dir: Some(PathBuf::from(var("LOG_DIR").unwrap_or("/data".into())))
            .filter(|dir| dir.is_dir()),
    };
    let interval = var("INTERVAL").and_then(|v| v.parse().ok()).unwrap_or(30);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into();
    cfg.log(format!(
        "watching {} -> {} every {interval}s, TMDB {}{}",
        src.display(),
        cfg.dst.display(),
        if cfg.token.is_some() { "on" } else { "off" },
        if cfg.dry_run { ", dry run" } else { "" },
    ));

    // Entry -> (signature, already handled). An entry is handled once its
    // signature is the same in two scans in a row.
    // shortcut: a manual copy that stalls longer than INTERVAL looks finished,
    // raise INTERVAL if a hand-copied movie ever arrives cut short.
    let mut seen = HashMap::new();
    let mut pruned_day = String::new();
    loop {
        // Once a day is enough.
        let today = timestamp(unix_now())[..10].to_string();
        if let Some(dir) = cfg.log_dir.as_ref().filter(|_| today != pruned_day) {
            let _ = prune_log(dir, unix_now());
            pruned_day = today;
        }
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
                                        true
                                    }
                                    // Not marked as handled: tried again on the next scan.
                                    Err(e) => {
                                        cfg.log(format!("{name}: error: {e}"));
                                        false
                                    }
                                }
                        }
                        _ => false,
                    };
                    now.insert(path, (sig, handled));
                }
            }
        }
        seen = now;
        thread::sleep(Duration::from_secs(interval));
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
            token: None,
            dry_run: true,
            log_dir: None,
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
            token: None,
            dry_run: false,
            log_dir: None,
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
            token: None,
            dry_run: true,
            log_dir: Some(dir.clone()),
        };
        cfg.log("hello".into());
        prune_log(&dir, now).unwrap();
        let text = fs::read_to_string(dir.join(LOG_FILE)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(lines[0], format!("{recent} recent"));
        assert!(lines[1].ends_with(" hello"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
