# movie-mover

Watches a download folder and moves each finished download into a library:

- a movie to `<letter>/Title (Year)/Title (Year).<ext>`
- an episode (a name with `S01E02`) to `<Show>/Season 01/<Show> - S01E02.<ext>`

- Title and year come from TMDB, or from the release name when TMDB is off or has no match.
- The letter folder ignores a leading The/A/An. Digits go to `0-9`, X/Y/Z to `X-Y-Z`.
- Subtitles (`srt sub idx ass`) move with the movie. The rest of the download folder is deleted.
- An existing movie folder is never overwritten.
- Left alone: movies without a year in the name, with no video, or with more than one file over 100 MB.
- Episodes: a season pack is moved file by file. A show folder that already exists is reused,
  whatever its case or punctuation. Without a series folder episodes are left alone.

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `SRC` | `/Download/complete` | Folder to watch |
| `DST` | `/film` | Movie library |
| `SERIES` | `/serie` | Series library |
| `TMDB_TOKEN` | empty | TMDB "API Read Access Token". Empty turns TMDB off |
| `INTERVAL` | `30` | Seconds between scans |
| `IDLE_MINUTES` | `60` | Fall asleep after this long with nothing new. `0`: never |
| `PORT` | `8767` | Port of the start/stop page |
| `DRY_RUN` | empty | Any value except `0`: log the moves, change nothing |
| `LOG_DIR` | `/data` | Folder for the log file. Not mapped: log to stdout only |
| `LOG_DAYS` | `60` | Days a log line is kept |

An entry is processed when its size and timestamps are the same in two scans in a row.
The log goes to stdout and to `LOG_DIR/movie-mover.log` (times in UTC).
Lines older than `LOG_DAYS` are removed.

A manual copy that stalls longer than `INTERVAL` looks finished; raise `INTERVAL` if that happens.

## Start and stop

The service is awake when the container starts. Asleep it scans nothing.

- `http://<nas>:8767/start` wakes it, `/stop` puts it to sleep, `/` shows the state and the last log lines.
- The page has no password: anyone on the network can use it.

## Install (Synology)

1. Container Manager > Project > Create, paste `compose.yaml`.
2. Check the volume paths and the `user:` line. Set `TMDB_TOKEN`.
3. Start it and read the log. Remove `DRY_RUN` when the planned moves look right.
