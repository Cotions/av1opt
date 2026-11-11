# av1opt

Shrink video libraries by re-encoding to AV1 at a target perceptual quality (VMAF),
instead of one fixed setting for everything. Built on stock FFmpeg with SVT-AV1 and libvmaf.

Five commands:

- `scan` — read-only survey of a library: what each file is, what re-encoding would roughly save.
- `dups` — read-only search for videos that are kept more than once.
- `encode` — per-file CRF search against a VMAF target, full encode, verification, report.
- `bench` — which preset and SVT-AV1 parameters pay off on your own videos, measured.
- `replace` — once you are happy with the outputs: each takes its original's place, the
  original goes to the trash.

## Requirements

- FFmpeg built with `libsvtav1` and `libvmaf` (for example the BtbN static builds).
  Point av1opt at it with `AV1OPT_FFMPEG` / `AV1OPT_FFPROBE`, or `--ffmpeg` / `--ffprobe`.
- Rust (to build): `cargo build --release`.
- Optional: `systemd-run --user` for per-encode memory caps.

## scan

```
av1opt scan /videos /more/videos --csv report.csv
```

Probes every video in parallel (results cached in `~/.cache/av1opt/`), sorts each file into
*candidate* or a skip reason (already AV1, Dolby Vision, too short, low expected gain), and prints totals
by codec and folder plus the biggest wins. Savings are a rough metadata-based estimate shown as a
range; `encode` measures the real numbers. Never writes to the scanned folders.

Every successful `encode` appends its source and output bitrates to
`~/.local/share/av1opt/history.jsonl`. `scan` fits its size model to that history (a log-linear
fit of AV1 bits per pixel against source bits per pixel and resolution, pulled toward the
built-in pilot model while there are few encodes), so estimates and their ranges tighten as
you encode more.

`scan` also says how long the candidates would take to search and encode (`--preset`, default
6) and lists the time per file; `--sort per-hour` orders the top candidates by bytes saved per
hour of encoding, the place to start when the library is larger than your patience. The time
is seconds per frame by frame size: a guess from one six-core machine at first, then fitted to
the encode times in the history, so it becomes this machine's speed after a few encodes.
The CSV has it as `est_encode_secs`.

## dups

```
av1opt dups /videos /more/videos [--prefer /videos/keep] [--avoid trash] [--csv dups.csv]
```

Finds videos that show the same thing: the same video at another resolution, bitrate, codec or
container, inside black bars, and copies with the start or the end cut off. Nothing is moved or
deleted by the command; the result is a list of groups with the copy worth keeping first. (The
UI can move the copies you tick to the trash.)

How it works:

1. **Fingerprint** – a handful of frames are decoded at fixed times counted from the start
   (1.5 s, 4 s, 10 s, 20 s, 45 s, ... up to half the length) and the same times counted back
   from the end. Each frame is shrunk to a small grayscale picture, black bars shared by the
   video's frames are cut away, and a 63-bit perceptual hash is taken. Black and plain frames
   are left out. Fingerprints are cached in `~/.cache/av1opt/`, so only new or changed files
   are decoded on later runs (`--refresh` reads everything again).
2. **Compare** – every video against every other. Two frames are the same picture when their
   hashes differ in at most `--tolerance` bits (default 10 of 63; re-encodes of one video
   measure 0-6, unrelated videos 20 and more).
   - *Same video*: lengths within 2% (or 1.5 s) and at least three quarters of the frames
     from both ends agree.
   - *Same start* / *same end*: lengths differ, but the frames counted from that end agree
     (at least 3 in common, 80% of them matching). The longer file is listed first.
3. **Suggest** – within a group, the copy to keep is the one that is not under an `--avoid`
   text, then the longest (for cut copies), the highest resolution, the best bitrate for its
   codec (AV1 counts double H.264, HEVC and VP9 one and a half), a `--prefer` folder, the bigger
   file. The reason is printed next to it. Files with the same size and the same bytes at the
   start, middle and end are marked as identical.

The first run decodes about a dozen frames per video with accurate seeking: a few seconds of
one core each, so a large library takes hours once (`-j` sets how many are read at once).
`--csv` writes one row per file of every group, `--json` the full result.

Limits: a clip cut out of the middle of a video, a copy cut at both ends, and cuts of very
short clips (fewer than three frames in common) are not found. Videos that share a long
identical intro or credits can show up as *same start* or *same end*; that is why these are
listed apart and Compare is one click away.

## encode

```
av1opt encode movie.mkv some/folder -o /out [--target-vmaf 93] [--preset 6] [--container mp4|mkv]
```

For each file:

1. **Sample** – cuts up to 4 clips spread through the video into lossless, constant-frame-rate
   references (deinterlaced and cropped to even size when needed).
2. **Search** – finds the highest CRF whose mean VMAF over the samples meets the target. The
   first CRF tried is a guess: 35 for VMAF 93, moved by what your own past encodes say about
   bits per pixel and frame size (the same history `scan` learns from). The second goes where
   a curve of the usual shape through the first score crosses the target (scores fall faster
   the higher the CRF), every further one where the line through the two nearest measurements
   does, and the search stops as soon as that crossing lies close enough to measured CRFs to
   be trusted. On the measured curves of eight test clips (every CRF from 22 to 46, targets
   88 to 97) that is 2.2 trial encodes on average from the fixed guess and 2.0 from a guess
   within 3 CRF of the answer, never more than 4, and the CRF chosen is never more than 0.05
   VMAF short of the target. The answer is in SVT-AV1's quarter-step CRF. 4K uses the
   `vmaf_4k` model.
3. **Decide** – skips files that are already AV1, Dolby Vision, can't reach the target, or would
   save less than `--min-savings` (15%). The search stops at the first trial that misses the
   target while already larger than that: the CRF the target needs would be larger still.
   HDR10 and HLG are encoded with their colour signalling
   and mastering/light-level metadata intact (VMAF is then measured on the HDR signal, which
   tracks encoding damage but is not calibrated like SDR scores).
4. **Encode** – SVT-AV1, 10-bit, `--preset` 6 with `tune=0` unless `--svt-params` or `--tuned`
   say otherwise (see [bench](#bench)). Audio already in AAC/Opus/MP3 is copied, anything else becomes
   Opus. `--audio shrink` also re-encodes copied tracks that spend more than 1.5 times what
   Opus needs (96 kbps stereo, 56 mono, 256 for 5.1): a 320 kbps MP3 or 256 kbps AAC track
   becomes 96 kbps Opus, a 128 kbps one is left alone. It is a second lossy pass, so it is
   off unless asked for; the report notes every track it changed. MP4 output keeps text subtitles (as mov_text) and plays in Chrome and Firefox; use
   `--container mkv` to keep image subtitles too. Cover art is kept (MP4: attached picture,
   MKV: attachment with its original file name).
5. **Verify** – codec, duration, stream and cover counts, HDR transfer, plus VMAF spot checks at positions the search
   did not sample. Nothing is written unless all checks pass.
6. **Report** – the output plus `<name>.av1opt.json` with every CRF tried, scores, sizes and timings.

### Metadata

The source's tags (title, description, dates, ...) and chapters are copied by default;
`--strip-metadata` drops them. Stream languages and track names always stay, since players use
them to pick audio and subtitle tracks. `--keep-dates` gives each output the source's
modification time.

Tags can be added from a preset or one by one:

```
av1opt encode downloads/ -o /out --meta-preset youtube
av1opt encode show/ -o /out --meta-preset tv --meta 'show=My Show' --meta network=
```

| Preset    | Tags                                                                              |
|-----------|-----------------------------------------------------------------------------------|
| `youtube` | what yt-dlp `--embed-metadata` writes: title, date (upload date), description, synopsis, purl and comment (video URL), artist (uploader), album, genre, show, season/episode, ... |
| `movie`   | title, date (year), genre, description, synopsis, comment                         |
| `tv`      | show, season_number, episode_sort, episode_id, title, network, date, description  |
| `music`   | title, artist, album_artist, album, track, genre, date                            |

Values are templates: `{field}` is filled from a yt-dlp info file next to the video
(`<name>.info.json`, as written by `--write-info-json`), else from the source's own tags;
`{a|b}` takes the first field with a value, `{file}` is the file name without extension, and
`{{`/`}}` are literal braces. A tag whose fields are all missing is skipped, and `--meta key=`
removes a preset tag. `--no-info-json` ignores info files. MP4 stores only the common tags
(title, artist, album, date, comment, genre, description, synopsis, show, episode/season
numbers, network, ...); others, like yt-dlp's `purl`, are reported in a note. MKV keeps any tag.
The tags written are listed in the `.av1opt.json` report. In the web UI the same options are in
the encode dialog, with **Preview tags** for a single file.

### Removing sections (`--remove`)

```
av1opt encode talk.mp4 -o /out --remove 0:00-0:42 --remove 12:03.5-14:10 --remove 58:30-end
```

Leaves those parts out of the output (one input file only). Times are `h:mm:ss`, `m:ss` or
seconds, with decimals; `end` (or nothing after the dash) means the end of the video. Sections
may overlap and come in any order. The video is re-encoded anyway, so the cuts are
frame-accurate: the kept parts are trimmed from the decoded source and joined (ffmpeg
`trim`/`concat`). Audio is cut the same way and re-encoded to Opus; text subtitles and chapters
are moved to their new times (lines and chapters inside removed parts go); image subtitles
(PGS, DVD) are dropped. Sample clips and spot checks are taken from kept parts only. A cut is
never skipped for being AV1 already or saving too little, and if the VMAF target is out of reach
it is encoded at the best CRF tried. Per-scene mode isn't combined with cuts (one pass instead).
The report lists the removed sections; an existing output with other sections removed is not
overwritten. In the web UI, **Choose sections to remove…** in the encode dialog (one file) has
a player with Mark start / Mark end (keys I and O), frame stepping, and playback that skips the
marked parts; Compare maps the cut output back onto the source.

### Per-scene CRF (`--scenes`)

One CRF for a whole video spends too many bits on easy scenes and too few on hard ones. With
`--scenes`, after the search above:

1. Scene changes are detected (`scdet`) and the video is cut into chunks of 4-20 s (10 s for 4K)
   at scene boundaries, frame-exactly, as lossless constant-frame-rate references.
2. Each chunk gets its own CRF from 2-3 fast probe encodes (preset +4), corrected by the measured
   VMAF gap between the probe preset and the real preset.
3. Each chunk is encoded at the real preset and measured; a chunk more than 1 VMAF off target is
   redone once with a corrected CRF.
4. Chunks are joined into one AV1 stream and muxed with the source's audio, subtitles, chapters
   and metadata. Verification additionally checks the exact frame count.

On a 60 s test clip mixing six kinds of footage, `--scenes` gave a 6% smaller file at identical
full-length VMAF (93.07), with per-segment VMAF between 91.9 and 94.1 instead of 90.8 to 95.9.
On a single continuous shot it gives no gain. It takes about 1.5-2x as long; `--workers 2`
encodes two chunks at once (each ffmpeg then gets half of `--memory-limit`).

Finished chunks are kept in `.av1opt-work-*` in the output folder, so an interrupted run resumes
where it stopped (including the search). Variable-frame-rate sources are encoded in one pass,
since chunking needs a constant frame rate.

Folders are walked recursively and their layout is mirrored under `--out-dir`. Every result is
appended to `av1opt-log.tsv` there; re-running the same command resumes: finished files are
skipped, and files skipped for a reason (low savings, ...) are not searched again with the same settings (target, preset, CRF range, `--min-savings`, `--force`) unless
`--retry-skipped` is given. `--order per-hour` encodes the files with the largest estimated
saving per hour of encoding first (`--order savings`: the largest saving first), going by the
same estimates as `scan`, so a long run stopped early has saved the most for its time; in the
web UI files are queued in the order of the scan table. Trial encodes are remembered in `~/.cache/av1opt/trials-v1.jsonl`
for the same file (path, size, date), sample clips, settings and ffmpeg build: an encode that
was stopped or failed, or is run again with another target, container or `--min-savings`,
starts from the scores it already has and often measures nothing new. `--fresh-search`
measures everything again. If two inputs would produce the same output name (same file name
from two input folders), the second is reported as a name collision instead of being skipped.

## bench

```
av1opt bench /videos [--presets 4,6,8] [--params tune=0 --params tune=2:enable-qm=1] [--save]
```

Answers "would another preset or SVT-AV1 parameter give me smaller files, and what would it
cost in encode time?" on your own videos instead of someone else's test clips. Read-only for the
videos.

1. **Pick** – up to `--max-videos` (6) videos, spread evenly over the folders given and over
   three size classes: 720p and below, 1080p, above 1080p (by pixel count, so portrait and
   cropped videos count with their landscape peers). Dolby Vision and very short files are left out.
2. **Measure** – `--samples` (2) lossless clips of `--sample-secs` (6) seconds per video. Every
   setting (each preset with each parameter set, plus the default: preset 6 with `tune=0`) gets
   the same CRF search as `encode`, so each ends at the target VMAF; every sample encode is timed.
3. **Compare at equal quality** – a setting's size and time at exactly the target are read
   between the last CRF that passes and the first that fails. Settings are then compared with
   the default per video and averaged as ratios (geometric mean), for all videos and per size
   class. A setting that fails or cannot reach the target on a video the default manages is
   shown with a note and never picked.
4. **Pick winners** – *smallest* (whatever the time), *balanced* (smallest within 10% of the
   default's encode time) and *fastest* (within 2% of the default's size). Differences under 1%
   count as none: the default stays.

The table lists every setting with size and time against the default, the best and worst video,
and marks the ones no other setting beats on both counts. `--csv` writes one row per video and
setting, `--json` everything including each sample encode.

`--save [balanced|smallest|fastest]` stores the winner of each size class in
`~/.local/share/av1opt/tuning.json`; `av1opt encode --tuned` then takes preset and parameters from
there by the video's size (classes without an entry use `--preset` and `--svt-params`).
`av1opt tuning` shows what is stored, `av1opt tuning --clear` forgets it. Without `--tuned`
nothing changes: `encode --svt-params key=value:key=value` sets parameters by hand.

Sample encodes are remembered in `~/.cache/av1opt/bench-v1.jsonl` (by file, clip layout,
encoder build, preset, parameters and CRF), so a stopped run carries on where it was and adding
a setting later only measures the new one. `--refresh` measures again.

Read the result with these in mind:

- **VMAF is the judge.** Settings tuned for metrics (`tune=1`, `tune=2`) tend to win on it;
  `tune=0` aims at how the picture looks to people, which VMAF only partly sees. Compare a clip
  by eye (the UI's Compare) before switching away from it on a few percent.
- **Times are wall-clock on this machine**, including reading the lossless clip. Run it on an
  otherwise idle machine; small differences in time are noise.
- **A dozen seconds per video is a sample.** More videos (`--max-videos`) say more than more
  settings; the per-video range in the table shows how much the videos disagree.
- Parameter names SVT-AV1 doesn't know are refused up front: ffmpeg would only warn and encode
  without them.

## replace

```
av1opt replace ~/av1opt-out                # lists what would happen, changes nothing
av1opt replace ~/av1opt-out --apply        # does it
av1opt replace ~/av1opt-out --undo [--apply]
```

`encode` leaves the library as it was: outputs go to their own folder, so no space is saved
yet. `replace` is the second half, for when you have looked at the
results: each original is moved to the trash and its AV1 file gets its name (with the
output's extension) in its folder. Name output folders, single reports (`.av1opt.json`) or
single output files.

- Without `--apply` it only lists: what can be replaced with sizes, what stays and why, and what
  was replaced earlier.
- An output is only used while its report still matches both files: the original and the
  output must be there with the sizes the encode recorded, and the output must be the smaller
  one. Originals that are links, reports without full paths (very old ones) and a new name
  that is already taken in the original's folder are refused too.
- Nothing is deleted and no file is written over. The original goes to the trash of the drive
  it is on (as in the duplicates view), and the space is freed when you empty the trash. If the
  output can't be put in place after that, the original is taken back out of the trash.
- An output on another drive than the original is copied next to it under a hidden name
  first (after a look at the free space), so a copy that fails leaves the original alone; the
  file in the output folder is removed once the copy has its final name. On the same drive it
  is a rename and takes no time.
- The new file gets the original's permissions and keeps the output's date. Subtitles, covers
  and other files next to the original are not touched: they still match by name, unless a
  player wants the extension to be the same.
- The report stays in the output folder and records where both files went (`replaced`: the
  time, the output's old path, the place in the trash). `--undo` uses that to put things back
  while the original is still in the trash: the AV1 file returns to the output folder, the
  original to its place. After the trash is emptied there is nothing to undo.
- Originals on ZFS are left alone unless `--allow-zfs` is given.
- A later `encode` or `scan` over the library sees the replaced files as AV1 and skips them.

## ui (optional)

```
cargo build --release --features ui
av1opt ui [--port 8765] [--no-open] [--memory-limit 12G]
```

A local web app in the browser (Chrome or Firefox), built from the same scan and encode code:

- **Scan**: pick folders, see per-file and total savings estimates, select candidates and queue them.
  **Time** is the estimated search and encode time of a file and **Per hour** its saving per
  hour of encoding (click to sort); the selection line adds up both for the ticked files.
- **Duplicates**: the `dups` search with progress and a Stop button. Each group shows a frame
  of every copy, the suggested keep and why; **Compare** opens two copies in the split player.
  Tick the copies you don't want, then **Move to trash** (after a confirmation), or copy or save
  the list of paths. Nothing is deleted: each file is renamed into the trash of the drive it is
  on (`~/.local/share/Trash`, or `.Trash-<uid>` at the top of another drive, as file managers
  do), so it can be put back until you empty the trash, and no space is freed before that.
  The last copy of a video is never trashed, files that changed since the search are refused,
  and a drive without a usable trash gives an error instead of a copy to another disk.
- **Encode dialog**: target VMAF, preset, container, encoder parameters, metadata, sections
  to remove, and **Shrink heavy sound tracks to Opus** (`--audio shrink`, off by default).
- **Queue**: encodes run one at a time with live progress: CRF search trials, chosen CRF,
  the estimated time left per job and for the whole queue (files no scan has seen are
  estimated once they are probed),
  per-chunk results in scene mode, percent, fps and ETA. Cancel stops ffmpeg at once; finished
  scene chunks are kept, so queueing the same files again resumes. Unfinished jobs are
  re-queued automatically the next time `av1opt ui` starts. **Pause** holds the queue until
  you resume, and **Hours…** sets the hours of the day encodes may run in (for example 23:00
  to 07:00). Either way nothing new starts and the running encode is held where it is: its
  ffmpeg processes are stopped, not ended, so they use no processor but keep their memory,
  and they go on from the same frame afterwards. **Run now** lets the queue work outside the
  hours until they next begin. The pause and the hours are kept across restarts, and the
  time spent held is left out of the timings av1opt learns its estimates from.
- **Compare**: source and AV1 output on top of each other with a split slider, synced playback,
  frame stepping and zoom, plus the per-chunk VMAF timeline from the `.av1opt.json` report
  (click to seek). Sources the browser can't decode (HEVC in some browsers, MPEG-2, ...) are shown as a
  20 s near-lossless H.264 preview made on demand in `~/.cache/av1opt/previews`, never next to
  the source; MKV outputs the browser won't open are remuxed to MP4 there too. The **Original**
  panel replaces this one original by its output (after a confirmation that shows the new
  path), and puts it back afterwards for as long as it is in the trash; the comparison then
  plays the copy in the trash.
- **Benchmark**: the `bench` comparison with progress and a Stop button, results while it
  runs, and a chart of size against encode time per size class (hover a dot for its numbers;
  the table below has them all). "Use for…" on a row makes that setting the tuned one for the
  size class; the encode dialog then offers "Use the benchmark's settings by video size".
  Encodes wait while a benchmark runs and a benchmark won't start during an encode, so the
  timings mean something. The last result is kept in `~/.local/share/av1opt/bench-last.json`.
- **History**: every report found in the output folders used so far (and folders you add).
  **Replace originals…** lists the outputs that can take their original's place (the rules of
  `replace` above), all ticked; it works through the ticked ones one by one and keeps the ones
  that failed on screen with the reason. Replaced encodes carry a "replaced" mark. An original
  that is still in the encode queue is refused.

The server listens on 127.0.0.1 only and prints a sign-in link with a random token (the browser
gets it as a cookie). Requests with a foreign Host or Origin are refused, and media are only
served for files you picked in the app. Output folders on ZFS are refused (no override in the UI);
replacing an original that is on ZFS needs a tick in the dialog, or the confirmation in Compare, which says so.
`AV1OPT_UI_TOKEN` sets the token (for a desktop wrapper loading the same page);
`AV1OPT_UI_DEV=<repo>/ui` serves the page files from disk while working on them.

**Settings** sets the ffmpeg and ffprobe programs (saved in `~/.local/share/av1opt/ui.json`,
winning over `--ffmpeg` / `AV1OPT_FFMPEG`) and checks them for libsvtav1 and libvmaf. While they
lack either, a banner says so and queued jobs wait instead of failing. `av1opt encode` runs the
same check before it starts.

Stopping the server (Ctrl-C, SIGTERM) stops running ffmpeg processes and keeps unfinished jobs
for the next start. When av1opt is killed outright (`kill -9`, a crash) its ffmpeg processes
are killed with it, held or not, so none is left writing a file nobody will finish.

### Desktop app (optional)

`desktop/` wraps the same page in an Electron window. Its bundled Chromium always plays AV1 and
H.264, whatever browser is installed. It starts `av1opt ui` with a random token, shows the page,
and stops the server when the window closes. If the window process dies, the server notices
(stdin closes) and stops too. Closing the window or pressing Ctrl+Q while jobs are running or
waiting asks first; logging out or SIGTERM quits without asking (jobs are kept). Folder
choices use the system dialog, and a notification says when a job ends while the window is in
the background. The server's messages are kept, with times, in
`~/.local/share/av1opt/desktop.log` (5 MB, then one older file `desktop.log.1`); Settings has
*Open log*. If the av1opt it runs speaks another API version than the app was built for, it
warns at start.

```
cargo build --release --features ui
cd desktop
npm install          # if npm skips electron's download script: node node_modules/electron/install.js
npm start            # runs ../target/release/av1opt
npm run dist         # dist/av1opt-desktop_<version>_amd64.deb with the av1opt binary inside
npm run install-desktop   # sudo apt installs it (/opt/av1opt, app menu entry, av1opt-desktop command)
npm run dist:appimage     # an AppImage instead, if wanted
```

The app uses, in order: `$AV1OPT_BIN`, the binary packed into the package,
`../target/release/av1opt`, then `av1opt` on PATH. Extra arguments go in `AV1OPT_UI_ARGS`
(for example `AV1OPT_UI_ARGS="--memory-limit 12G"`). A desktop launcher has no shell
environment, so set the ffmpeg path once in Settings.

### Safety

- `encode` never modifies, moves or deletes an input. Only `replace` (and the two buttons for
  it in the UI) moves originals, and only to the trash.
- Refuses to write to ZFS unless `--allow-zfs` is passed.
- Each ffmpeg run gets a memory cap (`--memory-limit`, default 40% of RAM) and low CPU priority
  (`--nice 10`), so a 4K encode can't freeze the desktop.
- Temp files live in a hidden folder in the output directory and are removed afterwards; leftovers
  from crashed runs are cleaned up on the next run.

### Saved files and versions

Reports, history lines, `ui.json`, `ui-queue.json`, `tuning.json`, benchmark results and the scene-mode work folders carry a
`format` number. Files from before versioning count as format 1. A newer av1opt keeps reading
older files; an older one skips reports and history lines from a newer format, leaves a newer
`ui.json` untouched, sets a newer `ui-queue.json` aside as `ui-queue.format<N>.json`, and starts
a scene-mode file over rather than resuming a work folder of another format.

### Debugging

`AV1OPT_DEBUG=1` prints every ffmpeg command. `--keep-temp` keeps samples and the partial encode.
`cargo test --features ui` also runs `tests/ui_api.rs`, which starts the real `av1opt ui` and
checks sign-in, request checks, media access, the job queue, its pause and hours, the duplicate search (and what its
trash action refuses), replacing an original and putting it back (and which reports may do that)
and the benchmark's requests and tuned settings over HTTP (no ffmpeg needed).

## Lessons baked in

- **Compare frames by index, not timestamp.** Encoders and muxers shift start times; libvmaf's
  timestamp sync then compares neighbouring frames and reports nonsense (60-75 instead of 98).
- **Snap to the closest standard frame rate.** Treating 24 fps as 23.976 drops a frame every ~42
  and misaligns comparisons.
- **Pre-roll before cutting.** Seeking straight into MPEG-2/TS or open-GOP H.264 can return frames
  from the wrong place; seek 2 s early and discard the warm-up after decoding.
- **Don't cut on a frame, or half-way between two.** Matroska stores timestamps in
  milliseconds, so a frame at 2.2 s in an MP4 source is at 2.200 or 2.199 s in its MKV output. A
  cut exactly between two frames then put every output frame on the edge between two slots of
  the constant-rate grid: every third frame came out doubled and the next one dropped, in the
  output only, and a good encode failed its spot check with VMAF 70 (3 of 40 cut points tried
  on one clip; 0 of 40 since). Cuts are now made a quarter of a frame before a frame, and time
  is counted from the first frame kept.
- **Score every 3rd frame, not every 2nd.** SVT-AV1's hierarchical GOP alternates better and
  worse frames; an even VMAF stride measures only one kind (+0.2 VMAF bias).
- **CRF has quarter steps.** `-svtav1-params crf=35.25` works; integer CRF alone left every
  chunk ~0.3 VMAF above target.
- **Cover art can be a plain video stream.** Pick the real video by codec, not "first video stream".
- **One SVT-AV1 encode already fills the CPU.** At preset 6 a single encode keeps 7-8 of 12
  threads busy from 720p to 4K (measured on a 6-core Ryzen 5 5600X); four sample encodes at
  once finished only 13-18% sooner than one after another, with four times the memory. The
  time went to the search instead: 4-5 trials per file, 2-5 times the final encode on short
  videos. So trials are saved (a better first guess, stopping as soon as the crossing is
  pinned down, remembering trials) rather than run side by side.
- **The score falls faster the higher the CRF, in much the same way on every clip.** On
  eight clips measured at every CRF from 22 to 46 (preset 6) a CRF step cost 0.15 VMAF at
  CRF 22, 0.45 at 35 (0.3 to 0.6 depending on the clip) and 0.65 at 44. Where the curve sits
  differs more: VMAF 93 is crossed between CRF 28 and 39, and neither bits per pixel nor
  frame size told where better than the average did. So the search knows the shape and
  measures the place. A step along that shape instead of a
  straight 0.4 per CRF took the search from 2.4 to 2.2 trials, and `bench`'s search (which
  goes on until two neighbouring CRFs are measured) from 3.6 to 3.1.
- **ffmpeg ignores `-svtav1-params` names it can't parse.** A typo only prints a warning and the
  encode runs without the option, so parameters are tried on a few generated frames first.

## Tests

`cargo test` covers the CRF search (exactness against brute force, trial count and misses on
the measured curves of eight real clips, starting from scores known beforehand), where clips are cut relative to frames, the first-CRF guess and what it learns from past encodes, the
encode time estimate (the first guess, learning a faster machine, other presets), the
trial cache (what counts as the same file, clips and settings), which sound tracks are copied
or shrunk, chunk CRF
estimation, scene chunk planning, sample placement, frame-rate snapping and source filters, plus
the duplicate finder's hashes (scaling, noise, black bars, flat frames), pair rules and keep
suggestions, and the benchmark's arithmetic (size and time at the target, comparison across
videos, picks, spreading the test videos), and `replace`: the round trip with `--undo`, an
output with the original's own extension, an output on another drive, every refusal (and that a
refusal moves nothing), a trash that fails, a name taken at the last moment, two runs on one
report at once.
