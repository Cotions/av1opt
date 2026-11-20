# scan

```
av1opt scan /videos /more/videos --csv report.csv
```

Probes every video in parallel (results cached in `~/.cache/av1opt/`), sorts each file into
*candidate*, *may save too little: tried last*, or a skip reason (already AV1, Dolby Vision, too
short, low expected gain), and prints totals by codec and folder plus the biggest wins. Savings
are a rough metadata-based estimate shown as a range; `encode` measures the real numbers. A
candidate is expected to save 15% of its video stream (`--min-savings`, the line `encode` holds
it to as well). A file expected to save less whose range still reaches 15% is worth a search
but comes after the others: orders by saving weigh every file by its chance of being kept. Low
expected gain means even the kind end of the range falls short. Never writes to the scanned
folders.

A walk through a folder, in `scan`, `dups`, `bench` and `encode`, looks at files whose names
end in `.mp4`, `.mkv`, `.m4v`, `.mov`, `.webm`, `.avi`, `.wmv`, `.flv`, `.ts`, `.m2ts`,
`.mts`, `.mpg`, `.mpeg`, `.vob` or `.3gp`, in any case. `encode` also takes a file of
another name when it is named on its own, as long as what is in it is a video format ffmpeg
reads.

## `scan` options

`av1opt scan <folders or files>...`

| Option | Default | What it does |
|---|---|---|
| `--ffprobe` | `ffprobe` (or `AV1OPT_FFPROBE`) | ffprobe program to use. |
| `-j`, `--jobs` | 6 | ffprobe runs at once (1-64); keep it low on slow or FUSE-mounted drives. |
| `--min-savings` | 0.15 | Share of its video stream (0-0.95) a file must be expected to save to be a candidate. |
| `--csv` | none | Write one row per file to this CSV file. |
| `--top` | 15 | Number of top candidates listed. |
| `--target-vmaf` | 93 | VMAF target the estimates assume. |
| `--preset` | 5 | SVT-AV1 preset the time estimates assume. |
| `--sort` | `savings` | Order of the top list: `savings` or `per-hour` (saving per hour of encoding). |
| `--hours` | none | Also list the candidates that save the most together in this many hours of encoding. |
| `--list` | none | Write the paths `--hours` picked to this file, one per line. |

Every run of ffprobe on a video, here and in the other commands, has a time limit that only
a stalled drive should reach: at most two minutes. A run that reads every packet of a file, as
counting its frames does, is stopped after two minutes without reading anything, or a minute
more for each gigabyte begun where its reading cannot be seen, and in any case once it has had
two minutes plus the time to read the file at 1 MB/s. A file ffprobe could
not finish, or that the system could not read for it, shows as unreadable in this scan and is
probed again by the next. A file ffprobe read and rejected is remembered until it changes.

Every successful `encode` appends its source and output bitrates to
`~/.local/share/av1opt/history.jsonl`, except a cut encoded below a target it could not reach,
whose sizes would mislead. `scan` fits its size model to that history (a log-linear
fit of AV1 bits per pixel against source bits per pixel and resolution, pulled toward the
built-in model while there are few encodes), so estimates and their ranges tighten as
you encode more. Past encodes are first brought to the preset and target asked about, and a
video unlike those encoded so far gets a wider range. The range of a total allows for files
erring in different directions, so it is narrower than their ranges added up.

`scan` also says how long the candidates would take to search and encode, in one pass and
with a CRF per scene (a multiple learned from the per-scene encodes made here), and with
`--scenes=auto` (the share of videos that end up per scene: a third at first, then what the
encodes made that way here say; the queue of the UI counts each file at that average)
(`--preset`, default 5) and lists the time per file; `--sort per-hour` orders the top candidates by bytes saved per
hour of encoding, the place to start when the library is larger than your patience. The time
is seconds per frame by frame size: a guess from one six-core machine at first, then fitted to
the encode times in the history, so it becomes this machine's speed after a few encodes.
The CSV has it as `est_encode_secs`.

`--hours 8` adds the candidates that save the most together in eight hours of encoding, and
`--list tonight.txt` writes their paths, one per line
(`xargs -d '\n' -a tonight.txt av1opt encode -o /out`). This is not the per-hour order cut
off at eight hours: a long video with the best rate can leave a gap that two shorter ones
would have filled, so every way of filling the time is tried (a knapsack over the time cut
in up to 10,000 slots; lengths are rounded up, so the choice never runs over).

What a command was asked to write (`scan --csv` and `--list`, `dups --csv` and `--json`,
`bench --csv`, `--json` and `--save`) is written before its results are printed, so a closed
pipe, as in `av1opt bench /videos --save | head`, loses none of it. A file that cannot be
written is reported after the results.
