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
built-in model while there are few encodes), so estimates and their ranges tighten as
you encode more.

`scan` also says how long the candidates would take to search and encode, in one pass and
with a CRF per scene (a multiple learned from the per-scene encodes made here), and with
`--scenes=auto` (the share of videos that end up per scene: a third at first, then what the
encodes made that way here say; the queue of the UI counts each file at that average)
(`--preset`, default 6) and lists the time per file; `--sort per-hour` orders the top candidates by bytes saved per
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

1. **Sample** – cuts clips spread through the video into lossless, constant-frame-rate
   references (deinterlaced and cropped to even size when needed). Where the whole video is
   scored afterwards (step 5, the usual case), the search only has to find the video's mean,
   and what throws that off is where the clips lie, not how long they are: one clip of 1
   second for every 8 seconds of video, 8 at least and 64 at most, spread evenly from the
   first second to the last. Where one CRF has to carry its worst stretch (`--scenes`, HDR,
   removed sections), clips of 4 seconds, long enough to show a stretch: 8 of them up to a
   quarter of an hour, then one for every two minutes more, 32 at most. `--samples` and
   `--sample-secs` set both by hand, and with them the 4-second scheme.
2. **Search** – finds the highest CRF whose mean VMAF over the samples meets the target. The
   first CRF tried is a guess: 35 for VMAF 93, moved by what your own past encodes say about
   bits per pixel and frame size (the same history `scan` learns from). The second goes where
   a curve of the usual shape through the first score crosses the target (scores fall faster
   the higher the CRF), every further one where the line through the two nearest measurements
   does, and the search stops as soon as that crossing lies close enough to measured CRFs to
   be trusted. On the measured curves of eight test clips (every CRF from 22 to 46, targets
   88 to 97) that is 2.2 trial encodes on average from the fixed guess and 2.0 from a guess
   within 3 CRF of the answer, never more than 4, and the CRF chosen is never more than 0.05
   VMAF short of the target. A trial encodes the samples end to end as one clip and scores
   them in one pass. The answer is in SVT-AV1's quarter-step CRF. 4K uses the
   `vmaf_4k` model. A sample 4 or more below the samples' mean gets a note that quality
   differs along the video (see [per-scene CRF](#per-scene-crf---scenes)). Where the whole
   video can't be scored afterwards (step 5: HDR, removed sections), one CRF has to carry
   its worst stretch: a spot check may read 4 VMAF below the target, and reads a little
   higher or lower than the samples did. If the worst sample at the chosen CRF is not a
   point above that mark there, the search goes on to the CRF that puts it there, and the
   report says so.
3. **Decide** – skips files that are already AV1, Dolby Vision, can't reach the target, or would
   save less than `--min-savings` (15%). The search stops at the first trial that misses the
   target while already larger than that: the CRF the target needs would be larger still.
   HDR10 and HLG are encoded with their colour signalling
   and mastering/light-level metadata intact (VMAF is then measured on the HDR signal, which
   tracks encoding damage but is not calibrated like SDR scores).
4. **Encode** – SVT-AV1, 10-bit, `--preset` 6 with `tune=0` unless `--svt-params` or `--tuned`
   say otherwise (see [bench](#bench)). Two parameters are added to whatever set is in
   use unless it names them itself: `keyint=10s` (a key frame every 10 seconds instead of
   every 5) and `enable-qm=1` (quantization matrices). Audio already in AAC/Opus/MP3 is copied, anything else becomes
   Opus. `--audio shrink` also re-encodes copied tracks that spend more than 1.5 times what
   Opus needs (96 kbps stereo, 56 mono, 256 for 5.1): a 320 kbps MP3 or 256 kbps AAC track
   becomes 96 kbps Opus, a 128 kbps one is left alone. It is a second lossy pass, so it is
   off unless asked for; the report notes every track it changed. MP4 output keeps text subtitles (as mov_text) and plays in Chrome and Firefox; use
   `--container mkv` to keep image subtitles too. Cover art is kept (MP4: attached picture,
   MKV: attachment with its original file name).
5. **Verify** – codec, duration, stream and cover counts, HDR transfer, and the picture.
   A video encoded in one pass is scored against its source along its whole length (every
   third frame, each against the source frame of the same number), in stretches of five
   seconds a second apart. One CRF gives a video one quantizer, not one quality, so some
   stretches end up well below the mean. Those more than 4 VMAF below the target are
   **encoded again**: the groups of pictures that hold them (each starts with a key frame
   and refers to nothing outside itself) are cut from the source, encoded at a lower CRF
   (up to three tries, going by what the stretch itself gains per step) and put in the place
   of the ones they replace. Every frame keeps its timestamp and the other streams are not
   touched; the joined file's frames are counted, and its first and last new stretch are
   scored once more where they now sit. A stretch that more bits don't lift (noise the
   metric reads as damage, mostly) is kept at the best it got. The report lists each
   stretch with its CRF, score and bytes before and after, and the mean and the lowest five
   seconds of the whole video. Windows left less than a point under the line after three
   rounds are kept and noted; stretches further short become
   samples of a second search (the worst sample two points above the line this time) and
   the video is encoded once more at the lower CRF it finds, which the report says; if that
   saves less than `--min-savings` the video is skipped, and a second failure stands.
   Where frames can't be paired one for one (removed sections, a source whose frames stay
   for unequal times), in HDR and for videos encoded per scene, it is spot checks at
   positions the search did not sample instead (five seconds at 30% and at 70% of the way;
   one for every ten minutes of a video of 25 minutes or more, eight at most; all of a
   video of ten seconds or less, once), and a spot check more than 4 below the target
   leads to the same second search. Nothing is written unless all checks pass.
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
   redone once with a corrected CRF. Some pictures score below the target however many bits
   they get (noise the metric reads as damage, mostly): when every probe of a chunk misses
   and the gain between them would still miss at `--min-crf`, the chunk is encoded at the
   highest CRF that scored within half a point of the best and not redone, and the report
   counts such scenes.
4. Chunks are joined into one AV1 stream and muxed with the source's audio, subtitles, chapters
   and metadata. Verification additionally checks the exact frame count.

On a 60 s test clip mixing six kinds of footage, `--scenes` gave a 6% smaller file at identical
full-length VMAF (93.07), with per-segment VMAF between 91.9 and 94.1 instead of 90.8 to 95.9.
On a single continuous shot it gives no gain. It takes about 1.5-2x as long on short clips
and about 3x on real 15-minute recordings (six of them: 180 minutes where one pass was
estimated at 58); `--workers 2` encodes two chunks at once (each ffmpeg then gets half of `--memory-limit`).

`--scenes=auto` spends that time only where it buys something: on videos whose samples score
far apart at one CRF (the worst one 4 VMAF or more below their mean), and one CRF for the
rest. (The numbers that follow were measured with four samples of eight seconds, where that
line was at 2.) Whole encodes of eight test clips, every frame scored against the source: on the six
whose worst sample was 0.3 to 1.6 below the mean, no five seconds of the encode fell more
than 3 VMAF below the target. On the two where it was 2.9 and 4.1 below, stretches fell 8.5
and 5.5 below the target (3-4% of all five-second windows were under what verification
accepts, so now and then a finished encode would have been thrown away), while other
stretches sat at 97. Encoded per scene, the worst five seconds of those two came out at 89.6
and 89.8 instead of 84.6 and 87.5 and the best at 96 instead of 97.5, for 15% and 7% more
bytes: the one-CRF encode of the first was 0.9 VMAF short of the target over the whole
video, which its four samples had not shown. Without `--scenes`, the report of such a video
says that quality differs along it.

Finished chunks are kept in `.av1opt-work-*` in the output folder, so an interrupted run resumes
where it stopped (including the search). Chunks are cut and compared at a constant frame
rate, and a file's header is not taken at its word for having one: the scene pass counts how
many frames that rate would drop or show twice. Past one in 10,000 the chunks are cut at the
frames' own timestamps instead, each frame is encoded once, and the joined video is stamped
with the times the source's frames had, to the tick (an MP4 keeps the source's time base;
Matroska counts in milliseconds as it always does). That is for frames shown about as long
as each other, as from a camera whose clock drifts. Where more than one step in a hundred is
under half the usual or over one and a half times it (a screen recording, a stream with
frames missing), a chunk's score would count the frames that stay longest no more than the
rest: such a file is encoded in one pass, with a note in the report, and so is one whose
timestamps repeat or run backwards.

Folders are walked recursively and their layout is mirrored under `--out-dir`: one folder's
contents directly below it, several folders each below a folder of their own name
(`encode -o out lib/Kay lib/Sofia` writes `out/Kay/...` and `out/Sofia/...`). Every result is
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

Sample encodes are remembered in `~/.cache/av1opt/bench-v2.jsonl` (by file, clip layout,
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
- A report is a file like any other, so it is only followed where `encode` writes one: next
  to its output, under the output's name. Both files it names have to be videos. A report
  that turns up in some other folder and points at files elsewhere moves nothing.
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
  original to its place. After the trash is emptied there is nothing to undo. The trash has
  to agree: a file goes back only if the trash's own record says it came from that place.
- Originals on ZFS are left alone unless `--allow-zfs` is given, and are put back there by
  `--undo` only with it.
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
  **Most saved in N h → Pick** ticks the candidates that save the most together in that many
  hours of encoding (`scan --hours`); it starts at the length of the encoding hours when
  those are set.
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
  re-queued automatically the next time `av1opt ui` starts. **Do next** on a waiting job
  starts it before the other waiting ones, and **Queue again** on a cancelled or failed job
  queues its files once more with the same settings. **Pause** holds the queue until
  you resume, and **Hours…** sets the hours of the day encodes may run in (for example 23:00
  to 07:00). Either way nothing new starts and the running encode is held where it is: its
  ffmpeg processes are stopped, not ended, so they use no processor but keep their memory,
  and they go on from the same frame afterwards. **Run now** lets the queue work outside the
  hours until they next begin. The pause and the hours are kept across restarts, and the
  time spent held is left out of the timings av1opt learns its estimates from.
- **Watched folders** (**Watch folder…** on the Queue page): videos that turn up in a folder,
  or anywhere below it, are queued on their own while av1opt is open, with the settings and
  the output folder the watch was given. The folder is looked at every five minutes (less
  often when walking it takes long: a look never takes a tenth of the time), not through
  file-change notifications, which network and FUSE mounts don't send. A video is queued
  once two looks found it unchanged and it has not been written to for two minutes, so a
  download still running is left alone, and only if a scan would call it a candidate: AV1,
  too short, too little to gain and unreadable files are left. The videos in the folder
  when it was added are not queued unless asked for. Queueing is all a watch does: the
  pause and the hours decide when the encode runs, and originals are never replaced or
  touched. What was seen is kept in `ui-watch.json`, and a video gone from the folder is
  remembered for a month, so a drive that was not mounted does not come back as a folder
  full of new videos.
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
  **Space saved over time** shows the total as it grew, a line that steps up on the days
  something was encoded (hover it, or use the arrow keys, for a day's numbers); it appears
  once there are encodes from two different days.
  **Replace originals…** lists the outputs that can take their original's place (the rules of
  `replace` above), all ticked; it works through the ticked ones one by one and keeps the ones
  that failed on screen with the reason. Replaced encodes carry a "replaced" mark. An original
  that is still in the encode queue is refused.

The server listens on 127.0.0.1 only and prints a sign-in link with a random token (the browser
gets it as a cookie). Requests with a foreign Host or Origin are refused, and media are only
served for files you picked in the app. Output folders on ZFS are refused (no override in the UI);
replacing an original that is on ZFS needs a tick in the dialog, or the confirmation in Compare, which says so.
Putting one back there, and moving duplicates on ZFS to the trash, ask in a dialog of their own.
The ffmpeg and ffprobe programs set in Settings have to be named like that (`ffmpeg`, `/opt/x/ffprobe-7`).
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
- An output file that appears in the output folder while its encode runs is not written
  over: the finished encode is kept next to it under another name, and the run says where.
- Each ffmpeg run gets a memory cap (`--memory-limit`, default 40% of RAM) and low CPU priority
  (`--nice 10`), so a 4K encode can't freeze the desktop.
- Temp files live in a hidden folder in the output directory and are removed afterwards; leftovers
  from crashed runs are cleaned up on the next run.

### Saved files and versions

Reports, history lines, `ui.json`, `ui-queue.json`, `ui-watch.json`, `tuning.json`, benchmark results and the scene-mode work folders carry a
`format` number. Files from before versioning count as format 1. A newer av1opt keeps reading
older files; an older one skips reports and history lines from a newer format, leaves a newer
`ui.json` untouched, sets a newer `ui-queue.json` or `ui-watch.json` aside as
`ui-queue.format<N>.json` (the videos in a watched folder then count as there before), and starts
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
- **A lossless clip is read far more often than it is written.** Every trial decodes each
  sample twice, as encoder input and as VMAF reference. FFV1 packs tightest but took 40 ms of
  processor time per 1080p frame to decode (150 ms at 4K), a fifth of a whole trial; FFVHUFF
  takes 8 ms (30 ms at 4K) for files twice the size (250 MB for 8 s of 1080p, 700 MB of 4K),
  which the trial never notices because they are read from memory. Trials got 13% shorter.
- **Encode the samples as one clip.** The encoder and libvmaf each start once per trial
  instead of once per sample, and per-sample scores come from libvmaf's per-frame log. That
  took another 11 to 16% off a trial. Checked frame for frame against the same stretches of a
  whole encode (four clips, two CRFs each), the mean over the samples came out 0.03 VMAF
  higher on average and at most 0.16 off; samples encoded one by one came out 0.04 lower
  and at most 0.13 off. Neither is the better predictor; one is faster. Together with the
  cheaper clips the search of four test files went from 140 s to 112 s (15 to 27% less
  each), with the chosen CRF within 0.75 of before.
- **The score falls faster the higher the CRF, in much the same way on every clip.** On
  eight clips measured at every CRF from 22 to 46 (preset 6) a CRF step cost 0.15 VMAF at
  CRF 22, 0.45 at 35 (0.3 to 0.6 depending on the clip) and 0.65 at 44. Where the curve sits
  differs more: VMAF 93 is crossed between CRF 28 and 39, and neither bits per pixel nor
  frame size told where better than the average did. So the search knows the shape and
  measures the place. A step along that shape instead of a
  straight 0.4 per CRF took the search from 2.4 to 2.2 trials, and `bench`'s search (which
  goes on until two neighbouring CRFs are measured) from 3.6 to 3.1.
- **A mean hides the stretches below it.** A CRF that brings the samples' mean to the target
  leaves a video's hard stretches below it: 8.5 VMAF below on one of eight test clips, whose
  samples averaged the target. The search knew, since one of its four samples scored 3
  below their mean, and did nothing with it; a generated clip with one hard scene was encoded
  in full and then refused by verification, again on every run, because the remembered
  trials led to the same CRF. The worst sample now counts: past what verification allows,
  the CRF comes down before the encode, and a video with samples far apart can be encoded
  per scene (`--scenes=auto`).
- **A Matroska header names a frame rate whatever the frames do.** ffprobe reports 24 fps as
  both the nominal and the average rate of an MKV whose frames come at 12, so comparing the
  two finds variable frame rates in MP4 only. Scene mode then cut such a file into
  constant-rate chunks: 96 frames became 190, every other one a repeat (a file with more
  frames than its header says would have lost some), and the checks passed because they
  compare at the same constant rate. ffmpeg's `fps` filter counts the frames it drops and
  repeats; scene detection runs it over the whole file anyway, so that count now decides.
- **A webcam's 30 fps drifts.** The first videos from a real library were 30 fps MP4s whose
  frames come a quarter of a frame late now and then: no gap, an average of 29.99, nothing a
  header shows. On the constant-rate grid of scene mode 8 frames of 27,895 were doubled, well
  under the one in 200 the count above let through. Every chunk met the target, and the
  joined video was thrown away after 32 minutes by a spot check of 87: a frame doubled inside
  the stretch checked sets every later frame against its neighbour. One frame in 10,000 is
  the limit now. Such a video first went to one pass with its own frame times, which was
  nearly all of that library; now scene mode itself goes by them: chunks are cut where the
  source's timestamps say, hold each frame once, and the joined video gets every timestamp
  back. The 15-minute video that started this, whole: 28,008 of 28,008 timestamps identical
  to the source's, 52 chunks at CRF 28.5 to 41.5, spot checks 92.6 and 90.8, 204 MB where
  the one pass that passed (CRF 33, after a first one at 38.75 had failed) took 248 MB.
- **Passed through is not kept.** In one pass such a video did not have its own frame times
  after all: ffmpeg passes timestamps through, but counts them in the encoder's units, and
  those are one frame of the declared rate unless it is told otherwise. Every frame was
  there, moved to the nearest whole frame (up to 6.7 ms off in two minutes of a real 30 fps
  recording, half a frame at most, no drift building up), which nobody sees, but a spot
  check sets the source's frames against the output's on a common grid: a generated clip
  where every frame differs from the last read 90.3 and 87.1 where its samples promised 93,
  and failed. The encoder now counts in the source's time base: timestamps identical,
  spot checks 93.1 and 93.1. On the real recording the difference was small (87.3 instead
  of 86.8 for the stretch that failed): its failures were real.
- **Not every uneven video is a drifting one.** A screen recording that shows some frames
  three times as long as others failed its spot check (87.9) when encoded per scene with
  its own frame times: a chunk's VMAF counts each frame once, the spot check counts each
  moment once, so a frame that stays three times as long weighs three times as much there.
  Scene mode by the source's timestamps is for frames of about one length; the rest keep
  their one pass.
- **Four samples miss things.** The same video in one pass then failed for real: its chunks
  had shown that a sixth of it needs a CRF of 29 to 32 where the samples asked for 38.75,
  and a spot check landed on such a stretch (86.8). Run again, it would have failed the same
  way for ever. The source's bitrate does not show where these are (a constant 5 Mbps; rank
  correlation with chunk difficulty 0.1). A failed spot check is now a sample of a second
  search, and the video is encoded once more.
- **Eight short samples see more than four long ones.** Three of 64 real videos failed
  their first spot check and were encoded twice. Searched again with other samples (the
  search alone, five ways, on those three and two that had passed): four of 8 seconds chose
  CRF 38.5, 40.25 and 40.25 as before; eight of 4 seconds, in the same search time, 33.5,
  35.75 and 37.75, where the second encodes had passed at 33, 36.25 and 35.75. Two of the
  three would have been right the first time and the third nearly. Sixteen got all three
  for twice the search time, which costs more than the second encodes it saves. A spot
  check looks at five seconds: a sample of four scores as low as it will, one of eight
  averages the hard moment away. So samples are 4 seconds now, 8 of them, and more with
  length. The worst of eight short samples lies further below their mean than the worst of
  four long ones (2.1 became 4.4 for the same video), so the line for "quality differs
  along the video" moved from 2 to 4: on 13 videos that marks the three failures (4.4, 4.6,
  5.1), one whose spot check had passed 1.3 above the line and one the four long samples had
  marked too, and none of the other eight (3.3 or less).
- **What two folders taught.** 64 videos, 32 GB, 1080p webcam recordings at a flat 5 Mbps:
  62 encoded (31.1 GB to 13.9 GB), 2 skipped as not worth it, none lost. Three failed a spot
  check in one pass (86.1, 86.8, 88.1) and all three passed on the second search and encode
  (CRF 4 to 6 lower). `scan`, on the pilot model alone, had called the 60 fps third of them
  not worth encoding (0% predicted, 37% measured) because the model took AV1's bits per pixel
  to be the same whatever the source spends; the pilot model now leans with the source.
  Encode times were predicted within 5% for one pass.
- **Two spot checks see ten seconds.** Four real 15-minute outputs that had passed
  verification, scored frame for frame afterwards: 29, 47, 78 and 104 of their roughly 900
  to 1,800 five-second windows were below the line a spot check draws (target - 4), the
  lowest at VMAF 82.2 to 85.8, around means of 92.9 to 95.1. Two of the four had already
  had their CRF lowered for a weak sample or a failed check, which had bought a higher mean
  and left the dips. A one-pass encode is now scored along its whole length, and the
  groups of pictures under the line are encoded again by themselves at the CRF each needs
  and put in place. The same two videos: 166.6 MB instead of 192.5 (mean 92.8 instead of
  94.7, lowest five seconds 89.0 instead of 82.8; 51 stretches, 9 of 15 minutes, at CRF 26
  to 40.25 around a video at 40.75) and 220.1 MB instead of 248.2 (mean 94.1 instead of
  95.1, lowest 89.0 instead of 85.8; 18 stretches, 3 minutes). A lower CRF for the whole
  video spends most of its bits where none were missing.
- **A stretch is judged where it sits.** The first version scored a stretch encoded again
  by itself. On a third video one group of pictures passed at every CRF tried (91.6 to
  92.5 over its own windows) while a window across its edge, part of it the neighbour's
  frames, stayed at 88.5 to 88.9 through three rounds; the video then went to a second
  search, which found that CRF 32 still left that window short at 91% of the source's
  size, and it was skipped, where the build before had kept it 38% smaller. The new
  frames are now set among the frames around them before they are scored, on the same
  windows the whole video is judged by. A stretch answers for its own part of a window
  that lies across its edge: judged by the whole window, the stretch next to a weak one
  was sent down to CRF 20.5 to make up for frames that were not its own, and
  gained 1.3 VMAF for it. A stretch whose gain per CRF step puts the line below the lowest
  CRF allowed is given up before the bytes are spent, and a window less than a point
  under the line after three rounds no longer costs the video. That video now: six
  stretches (65 seconds) encoded again in one round, lowest five seconds 89.5 where they
  were 86.1, 12.6 MB of 430 MB, and 9 of its 30 minutes of encoding spent on the check
  and the repairs.
- **Samples miss the mean by about a point.** Scoring the whole video shows what the
  search could not: at the CRF the samples chose for a mean of 93, three real videos came
  out at 93.8, 94.0 and 90.7 over their whole length. The low one is lifted by the
  stretches encoded again (90.7 became 92.8); the high ones keep the bytes they did not
  need. A second encode at a corrected CRF would get them back for
  twice the encoding time, and is not done.
- **A key frame every 10 seconds, and quantization matrices.** SVT-AV1 puts a key frame
  about every 5 seconds and leaves quantization matrices off. At equal VMAF, 10 seconds
  took 3.3% less on eight 10-second clips and 2.6% less on four of 40 seconds (20 seconds:
  3.2%, none at all: 3.8%, so most of the gain is in the first doubling, which keeps
  seeking quick and the stretches that can be encoded again short). Matrices took another
  1.1%, less on every clip. Both together: 4.4% on the eight clips they were chosen on and
  2.1% on six they were not (less on each of the six, by VMAF NEG and SSIMULACRA2 too),
  with encoding time within 2% either way. Tried the same way and left out: `tune=2` and `tune=5`, variance
  boost, encoding at a reduced resolution, and `tune=1` (better on some clips, worse on
  others).
- **A container's length is not the picture's.** One recording had 7:40 of video under 17:32
  of sound. Samples and spot checks were placed along the 17:32 and asked for frames that
  don't exist. They go by the video stream's own length now.
- **ffmpeg ignores `-svtav1-params` names it can't parse.** A typo only prints a warning and the
  encode runs without the option, so parameters are tried on a few generated frames first.

## Tests

`cargo test --test e2e -- --ignored` runs the real program with a real ffmpeg on clips
generated on the spot (a few seconds of test patterns, mostly 320x180; the whole set takes
a little over a minute): a plain encode and its report, the frames of the output against the source's one by one
(so a frame dropped, doubled or out of place is seen), subtitles, chapters, languages and
tags in both containers, 10-bit, HDR10 and HLG signalling, sound tracks copied, converted
and shrunk, removed sections, metadata presets and info files, interlaced, variable-rate,
rotated, odd-sized, 4:4:4, wide-pixel and full-range sources, cover art in every place it
can be, sound and picture offsets, AVI, WebM, MOV, MPEG and transport streams at six frame
rates, very short videos, awkward file and folder names, skips, failures and name
collisions, a scene-mode run killed half-way and resumed, an encode stopped half-way (nothing
left behind, no ffmpeg left running, trials remembered), `--scenes=auto` on an even and an
uneven video and the stretches of an uneven one that are encoded again without it (frame
times, sound and frame order of the joined file against the source), `replace` with `--undo` on a
real output, `scan` on a folder with a candidate, an AV1 file and a broken one (nothing in
the folder touched) and its `--hours` pick, `dups` on a smaller copy, a byte-identical copy and two cut copies, and
`bench` with its remembered sample encodes and `--save` feeding `encode --tuned`. With `--features ui` it also drives a real server: a paused queue (ffmpeg
stopped in place, the wait left out of the timings), cancelling a running and a held job,
queueing it again, closing the app in the middle of an encode and going on after the next
start, replacing an original from History, and a watched folder (a video there before, a
new one worth encoding, one in AV1 and one still being written). It needs an ffmpeg with libsvtav1, libvmaf, libx264 and libx265
(`AV1OPT_FFMPEG`, `AV1OPT_FFPROBE`), which is why a plain `cargo test` leaves it out.

`cargo test` covers the CRF search (exactness against brute force, trial count and misses on
the measured curves of eight real clips, starting from scores known beforehand), where clips are cut relative to frames, the first-CRF guess and what it learns from past encodes, the
encode time estimate (the first guess, learning a faster machine, other presets), the
trial cache (what counts as the same file, clips and settings), which sound tracks are copied
or shrunk, chunk CRF
estimation, scene chunk planning, sample placement, frame-rate snapping and source filters, plus
the duplicate finder's hashes (scaling, noise, black bars, flat frames), pair rules and keep
suggestions, and the benchmark's arithmetic (size and time at the target, comparison across
videos, picks, spreading the test videos), the pick for a number of hours (against trying
every subset, and on 30,000 videos), what a watched folder makes of the videos it finds, and `replace`: the round trip with `--undo`, an
output with the original's own extension, an output on another drive, every refusal (and that a
refusal moves nothing), a trash that fails, a name taken at the last moment, two runs on one
report at once.
