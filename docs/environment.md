# Environment

A variable set to nothing counts as not set. Base folders that are not absolute paths are
ignored, as the XDG specification says, and the default under the home folder is used.

| Variable | What it does |
|---|---|
| `AV1OPT_FFMPEG`, `AV1OPT_FFPROBE` | The programs used where `--ffmpeg` and `--ffprobe` are not given. |
| `AV1OPT_HISTORY` | The file of past encodes the estimates learn from, instead of `~/.local/share/av1opt/history.jsonl`. It has to name a file: a folder is said once and no history is used. |
| `AV1OPT_DEBUG` | Any value prints every ffmpeg command. |
| `AV1OPT_UI_TOKEN` | The sign-in token of `av1opt ui`, at least 16 letters or digits, for a wrapper that loads the page itself. |
| `AV1OPT_UI_EXIT_WITH_STDIN` | `1` makes `av1opt ui` stop when its standard input closes, so that it ends with the program that started it. |
| `AV1OPT_UI_DEV` | A folder `av1opt ui` reads the page files from instead of those built in, while working on them. |
| `AV1OPT_PREVIEW_LIMIT_SECS` | Seconds the page lets ffmpeg take to make a playable preview, instead of 600. |
| `AV1OPT_BIN`, `AV1OPT_UI_ARGS` | For the desktop app: the `av1opt` binary to start, and extra arguments for `av1opt ui`. |
| `XDG_CACHE_HOME` | Caches (probes, fingerprints, CRF trials, benchmark measurements) go in `av1opt/` in it; default `~/.cache`. |
| `XDG_DATA_HOME` | The history, the record, the tuned settings and the page's settings go in `av1opt/` in it, and the trash is `Trash/` in it; default `~/.local/share`. |
