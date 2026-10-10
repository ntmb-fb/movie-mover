# movie-mover

Watches a download folder and moves each finished movie into a library as
`<letter>/Title (Year)/Title (Year).<ext>`.

- Title and year come from TMDB, or from the release name when TMDB is off or has no match.
- The letter folder ignores a leading The/A/An. Digits go to `0-9`, X/Y/Z to `X-Y-Z`.
- Subtitles (`srt sub idx ass`) move with the movie. The rest of the download folder is deleted.
- An existing movie folder is never overwritten.
- Left alone: entries without a year in the name, with no video, or with more than one file over 100 MB.

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `SRC` | `/Download/complete` | Folder to watch |
| `DST` | `/film` | Library folder |
| `TMDB_TOKEN` | empty | TMDB "API Read Access Token". Empty turns TMDB off |
| `INTERVAL` | `30` | Seconds between scans |
| `DRY_RUN` | empty | Any value except `0`: log the moves, change nothing |

An entry is processed when its size and timestamps are the same in two scans in a row.
A manual copy that stalls longer than `INTERVAL` looks finished; raise `INTERVAL` if that happens.

## Install (Synology)

1. Container Manager > Project > Create, paste `compose.yaml`.
2. Check the volume paths and the `user:` line. Set `TMDB_TOKEN`.
3. Start it and read the log. Remove `DRY_RUN` when the planned moves look right.
