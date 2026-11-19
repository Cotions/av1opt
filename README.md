# av1opt

Shrink video libraries by re-encoding to AV1 at a target perceptual quality (VMAF),
instead of one fixed setting for everything. Built on stock FFmpeg with SVT-AV1 and libvmaf.

VMAF is a score from 0 to 100 of how close an encode looks to its source, made to predict
what viewers would say. The default target, 93, is hard to tell from the source; near it, one
point more or less costs or saves about a sixth of the file. Five seconds of an output that
score more than 4 below the target (below 89 for the default) are encoded again where the
whole video is scored, and otherwise lead to a lower CRF or a refusal (see [encode](#encode)).

Eight commands:

- `scan` — read-only survey of a library: what each file is, what re-encoding would roughly save.
- `dups` — read-only search for videos that are kept more than once.
- `encode` — per-file CRF search against a VMAF target, full encode, verification, report.
- `bench` — which preset and SVT-AV1 parameters pay off on your own videos, measured.
- `tuning` — shows the settings `bench --save` stored for `encode --tuned`, or forgets them.
- `replace` — once you are happy with the outputs: each takes its original's place, the
  original goes to the trash.
- `adopt` — takes outputs that are not on av1opt's record (made before it kept one) onto it,
  so that the page can replace them too.
- `ui` — all of this in a local web app (built with `--features ui`).

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

### `scan` options

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
   (1.5 s, 4 s, 10 s, 20 s, 45 s, ... up to half the length), the same times counted back
   from the end, and five more spread over the middle (at 1/6 to 5/6 of the length). Trash
   folders are not searched. Each frame is shrunk to a small grayscale picture, black bars shared by the
   video's frames are cut away, and a 63-bit perceptual hash is taken. Black and plain frames
   are left out; a frame that cannot be read marks the file as damaged or incomplete, as does
   a file that ends before its stated length. Fingerprints are cached in `~/.cache/av1opt/`, so only new or changed files
   are decoded on later runs (`--refresh` reads everything again).
2. **Compare** – every video against every other. Two frames are the same picture when their
   hashes differ in at most `--tolerance` bits (default 10 of 63; re-encodes of one video
   measure 0-6, unrelated videos 20 and more).
   - *Same video*: lengths within 2% (or 1.5 s), at least three quarters of all frames agree,
     frames from both ends are among them, at least three middle frames could be compared and
     80% of those agree, and the agreeing frames show at least three different pictures. Clips
     under 3 seconds may not hold three pictures: they are the same only if every sampled frame
     agrees and their lengths match within one frame.
   - *Same start* / *same end*: lengths differ, but the frames counted from that end agree
     (at least 3 in common, 80% of them matching). The longer file is listed first, unless
     it is under an `--avoid` text and the other is not.
3. **Suggest** – within a group, the copy to keep is a whole one rather than a damaged or
   incomplete one; among copies of one video, the one with the most sound tracks, then the most
   subtitle tracks; then the one not under an `--avoid` text, the longest (for cut copies), the
   highest resolution measured inside any black bars (within 10% of the highest counts as the
   same), the best bitrate for its codec (AV1 counts double H.264, HEVC and VP9 one and a half;
   within 15% of the best counts as the same), a `--prefer` folder, the bigger file, and in the
   end the first by name. The reason and the differences that matter are printed next to it. If
   another copy holds tracks the suggested one lacks, or every copy is damaged, the group is
   shown for you to judge and none of its copies counts as spare. Files with the
   same size and the same bytes at the start, middle and end are marked as identical, and among
   those the name decides.

The first run decodes about 15 to 25 frames per video (19 for one of 10 to 15 minutes, 27 at
most) with accurate seeking: a few seconds of
one core each, so a large library takes hours once (`-j` sets how many are read at once).
`--csv` writes one row per file of every group, `--json` the full result.

Limits: a clip cut out of the middle of a video, a copy cut at both ends, and cuts of very
short clips (fewer than three frames in common) are not found. Nor is a copy so damaged that
its middle no longer agrees. Videos that hardly change (a fixed camera, a still picture) are
never called the same, because such videos cannot be told apart. Videos that share a long identical intro or
credits can show up as *same start* or *same end*; that is why these are listed apart and
Compare is one click away. An upscaled copy cannot be told from a sharper one at this cost: it
counts as the higher resolution, so compare before you trust that reason. Subtitles burnt into
the picture are not seen either: a copy with them groups with a clean one and can be the spare.

### `dups` options

`av1opt dups <folders or files>...`

| Option | Default | What it does |
|---|---|---|
| `-j`, `--jobs` | 6 | Videos read at once (1-64). |
| `--prefer` | none | Between equally good copies, keep the one under this folder. Repeatable; the first wins. |
| `--avoid` | none | Never suggest keeping a copy whose path contains this text if another copy exists. Repeatable. |
| `--tolerance` | 10 | Bits (0-20 of 63) two frame hashes may differ by and still be the same picture. |
| `--refresh` | off | Fingerprint every video again instead of using the cache. |
| `--top` | 20 | Number of groups listed. |
| `--csv` | none | Write one row per file of every group to this CSV file. |
| `--json` | none | Write the full result to this JSON file. |
| `--ffmpeg` | `ffmpeg` (or `AV1OPT_FFMPEG`) | ffmpeg program that reads the frames. |
| `--ffprobe` | `ffprobe` (or `AV1OPT_FFPROBE`) | ffprobe program to use. |
| `--memory-limit` | `auto` | Memory cap shared by the ffmpeg runs, written as for `encode`. |
| `--nice` | 10 | CPU priority (0-19; higher leaves more to the desktop). |

## encode

```
av1opt encode movie.mkv some/folder -o /out [--target-vmaf 93] [--preset 5] [--container mp4|mkv]
```

For each file:

1. **Sample** – cuts clips spread through the video into lossless, constant-frame-rate
   references (deinterlaced and cropped to even size when needed). An encode in one pass
   without `--scenes`, HDR or removed sections (the usual case) is scored along its whole
   length afterwards (step 5), so the search only has to find the video's mean, and what
   throws that off is where the clips lie, not how long they are: one clip of 1 second for
   every 8 seconds of video, 8 at least and 64 at most, spread evenly from the first second
   to the last; a video under 64 seconds is sampled whole, as clips of a second side by side.
   Where one CRF has to carry its worst stretch (`--scenes`, HDR, removed sections), clips
   of 4 seconds, long enough to show a stretch: 8 of them up to about 17 minutes, then one
   for every two minutes of video, 32 at most. `--samples` and `--sample-secs` set the
   number and the length by hand; what they leave out follows the 4-second scheme, and the
   search then goes by the samples as they are, whole-video check or not.
   A video whose first frame is not at zero (Matroska and MPEG-TS files can start late) is
   measured from where it starts; for a Matroska file that takes a look at its first and its
   last packets, a fraction of a second. Matroska states one frame's length, and ffmpeg
   reads that as both the declared and the average rate whatever the frames do; so the
   rate of a Matroska file is read off its first 32 packets, and where those come slower
   than declared, off 32 in its middle too (see the lessons below). Clips, and the chunks of a per-scene encode, are
   chosen by the source's own timestamps: where a seek lands past them (MPEG-TS, files
   without an index), the cut is redone from 30 seconds earlier and then from the start,
   which is slow on a long transport stream but takes the right frames. A file that stops
   before the length it claims is refused, and the message says that it ends early or is
   damaged.
2. **Search** – finds the highest CRF between `--min-crf` (20) and `--max-crf` (55) whose mean
   VMAF over the samples meets the target. The
   first CRF tried is a guess: 35 for VMAF 93, moved by what your own past encodes say about
   bits per pixel and frame size (the same history `scan` learns from). The second goes where
   a curve of the usual shape through the first score crosses the target (scores fall faster
   the higher the CRF), every further one where the line through the two nearest measurements
   does, and the search stops as soon as that crossing lies close enough to measured CRFs to
   be trusted. On the measured curves of eight test clips (every CRF from 22 to 46, targets
   88 to 97) that is 2.3 trial encodes on average from the fixed guess and 2.0 from a guess
   within 3 CRF of the answer, never more than 4. A trial encodes the samples end to end as
   one clip and scores them in one pass. With the 1-second clips of the usual case, the
   video's stretches more than 4 below the target are going to be encoded again (step 5),
   which lifts the mean; so every sample below the point 2 above that line counts as that
   point, about where such a stretch ends up, and the search aims at the mean the video will
   have when it is done, 0.2 short of the target because step 5 measures that mean and makes
   up what is missing. The answer is the step of SVT-AV1's quarter-step CRF nearest the
   crossing. That holds only where the frames of the source are shown about as long as each
   other, which is looked up in its packets before the search: elsewhere step 5 can only
   spot check, and the search aims at the target itself. In the other cases nothing makes
   up for a miss, so it is rounded toward quality,
   and is never more than 0.05 VMAF short of the target. 4K uses the
   `vmaf_4k` model. Clips of 1 second are too short to speak for a stretch; in the 4-second
   scheme a sample 4 or more below the samples' mean gets a note that quality differs along
   the video (see [per-scene CRF](#per-scene-crf---scenes)), unless `--scenes` gives every
   scene its own CRF anyway. Where the whole
   video can't be scored afterwards (step 5: HDR, removed sections), one CRF has to carry
   its worst stretch: a spot check may read 4 VMAF below the target, and reads a little
   higher or lower than the samples did. If the worst sample at the chosen CRF is not a
   point above that mark there, the search goes on to the CRF that puts it there, and the
   report says so.
3. **Decide** – skips files that are already AV1, Dolby Vision, can't reach the target, or would
   save less than `--min-savings` (15%). `--force` encodes the ones that are AV1 already or
   would save too little. The search stops at the first trial that misses the
   target while already larger than that: the CRF the target needs would be larger still.
   The samples speak for the video stream at one CRF; the finished file (with its
   stretches encoded again, its other streams and the container) is weighed once more:
   no smaller than its source, it is not kept (with `--force` or removed sections it is);
   smaller by less than `--min-savings`, it is kept with a note.
   HDR10 and HLG are encoded with their colour signalling
   and mastering/light-level metadata intact (VMAF is then measured on the HDR signal, which
   tracks encoding damage but is not calibrated like SDR scores). HDR10+ dynamic metadata is
   not kept: such a file plays as HDR10, and the report says so. They are encoded in one
   pass even with `--scenes`, with a note in the report: a video joined from scenes would
   lose that metadata from its container.
4. **Encode** – SVT-AV1, 10-bit, `--preset` 5 with `tune=0` unless `--svt-params` or `--tuned`
   say otherwise (see [bench](#bench)). Two parameters are added to whatever set is in
   use unless it names them itself: `keyint=10s` (a key frame every 10 seconds instead of
   every 5, counted in frames where a video's frames come at another rate than it declares) and `enable-qm=1` (quantization matrices). Audio already in AAC/Opus/MP3 is copied, anything else becomes
   Opus. `--audio shrink` also re-encodes copied tracks that spend more than 1.5 times what
   Opus needs (96 kbps stereo, 56 mono, 256 for 5.1): a 320 kbps MP3 or 256 kbps AAC track
   becomes 96 kbps Opus, a 128 kbps one is left alone. It is a second lossy pass, so it is
   off unless asked for; the report notes every track it changed. MP4 output keeps text subtitles (as mov_text) and plays in Chrome and Firefox; use
   `--container mkv` to keep image subtitles too. 608 closed-caption tracks become text
   subtitles (mov_text in MP4, ASS in MKV) and lose where they stand on screen; TTML is copied
   into MP4 and dropped from MKV, which has no place for it. In MP4, ASS subtitles lose their
   styles and attachments (fonts) are dropped; the report says so, and `--container mkv` keeps
   them. Cover art is kept: JPEG and PNG in MP4 (as
   attached pictures), JPEG, PNG, BMP and WebP in MKV (as attachments with their original
   file names). A cover of another kind is dropped, with a note in the report.
   The report also says what of the picture an encode to 10-bit 4:2:0 cannot keep: an
   interlaced source comes out as progressive frames, one per pair of fields; closed captions
   inside the picture are lost (the note gives the ffmpeg command that takes them out first);
   4:2:2 and 4:4:4 colour, 12-bit and up, and transparency are reduced. RGB is made YUV by the
   BT.709 matrix (BT.2020's where its primaries are those) and tagged with it. 3D and 360
   degree marks are kept; a video joined from pieces (after repair or with `--scenes`) keeps
   the 3D layout but loses the 360 degree mark, which ffmpeg cannot write there, with a note.
5. **Verify** – codec, duration, stream and cover counts, track names and languages, the
   shape the picture is shown at, where the sound starts against the picture (within a frame
   or 40 ms, whichever is longer), 3D and 360 degree marks, HDR transfer, mastering display
   and light level (an output whose container lost what the source says of them is refused),
   and the picture.
   A video encoded in one pass is scored against its source along its whole length (every
   third frame, each against the source frame of the same number), in stretches of five
   seconds a second apart. One CRF gives a video one quantizer, not one quality, so some
   stretches end up well below the mean. Those more than 4 VMAF below the target are
   **encoded again**: the groups of pictures that hold them (each starts with a key frame
   and refers to nothing outside itself) are cut from the source, encoded at a lower CRF
   (up to three tries, going by what the stretch itself gains per step) and put in the place
   of the ones they replace. Every frame keeps its timestamp and the other streams are not
   touched; the joined file's frames are counted, and its first and last new stretch are
   scored once more where they now sit. A new stretch goes in only where its sequence header
   says what the stream's does, apart from the superblock size and the level; one that says
   another range, say, is refused, and the video gets spot checks. One that differs in those
   two goes in only where the frame after it carries the stream's header again, as SVT-AV1
   writes one at every key frame; it is checked rather than trusted. A stretch at the very
   start that says a lower level than the stream is refused as well: MP4 and Matroska tell
   a player the level from the first sequence header, and would then say too little for
   the rest. A source is cut, encoded
   and scored in the range its first frame is decoded with: the video's own word where it
   has one, which ffmpeg and Chrome go by whatever range a remux wrote on the container
   (`-c copy -color_range pc` over a limited stream had every stretch refused before).
   No stretch goes more than 16 CRF below the video's.
   One that more bits don't lift (noise the metric reads as damage, mostly) keeps the
   smallest encode within half a point of the best it got, which can be the one it had
   already. The report lists each
   stretch with its CRF, score and bytes before and after, and the mean and the lowest five
   seconds of the whole video. The mean is checked too. Where it is under the target after
   the repairs, the lowest groups of pictures not yet touched are encoded again, lowest
   first, each only as far as it takes, until the measured mean reaches the target; what was
   repaired stays as it landed. That may take a quarter of the video at most, with one more
   round allowed to lift them half a point higher if the mean is still more than 0.05
   short; the report says how much was encoded again for the mean, at what cost, and whether
   it stayed short. The scores foretell all this by imitating the repairs before any is
   made: if the mean could not be brought up within that quarter, or would end more than 0.5
   above the target while no more than a quarter of the video is to be encoded again below the
   line, the whole video is encoded once more at a CRF moved by the difference, once per
   video. (Half a point over the target is about 6% more file than it needs; the second encode
   takes that off for up to twice the time on that video.) Windows left less than a point under the line after three
   rounds are kept and noted; stretches further short become
   samples of a second search (the worst sample two points above the line this time) and
   the video is encoded once more at the lower CRF it finds, which the report says; if that
   saves less than `--min-savings` the video is skipped, and a second failure stands.
   Where frames can't be paired one for one (removed sections, a source in which more than
   one frame in ten stays on screen much longer or shorter than the rest, as in a screen
   recording), in HDR and for videos encoded per scene, it is spot checks at
   fixed positions instead (five seconds at 30% and at 70% of the way;
   one for every ten minutes of a video of 25 minutes or more, eight at most; all of a
   video of ten seconds or less, once). So it is where stretches can't be encoded again (no
   room for them on the drive, say), and the report then says so. A spot check more than 4
   below the target leads to the same second search; a video encoded per scene or with
   sections removed fails instead. Nothing is written unless all checks pass. The source
   has to be as it was, too. It counts as changed if its size is different, or if its
   modification time is more than two seconds off and a fingerprint of its contents (a hash
   of its size and of a megabyte at its start, middle and end) differs as well; a file only
   touched passes. A changed source fails the encode: no output or report is kept, and
   neither are the chunks of a per-scene encode, which no longer belong to the file as it is
   now. A source that has gone away or cannot be read fails the encode too, but as that may
   only be a drive that dropped out, the chunks are kept and encoding it again goes on from
   them.
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
(PGS, DVD) and TTML are dropped. Sample clips and spot checks are taken from kept parts only. A cut is
never skipped for being AV1 already or saving too little, and if the VMAF target is out of reach
it is encoded at the best CRF tried. At least one second of the video has to stay: sections
that remove all of it, or all but a moment, are refused before the encode starts, and the web UI
refuses them when they are queued. A Dolby Vision file is skipped all the same, and sections
can't be removed from a file whose sound or subtitles run more than a second past its picture. Per-scene
mode isn't combined with cuts (one pass instead).
The report lists the removed sections; an existing output with other sections removed is only
overwritten with `--overwrite`. In the web UI, **Choose sections to remove…** in the encode dialog (one file) has
a player with Mark start / Mark end (keys I and O), frame stepping, and playback that skips the
marked parts; Compare maps the cut output back onto the source.

### Per-scene CRF (`--scenes`)

One CRF for a whole video spends too many bits on easy scenes and too few on hard ones. With
`--scenes`, after the search above (HDR videos and videos with sections removed stay in one
pass, and the report says so):

1. Scene changes are detected (`scdet`) and the video is cut into chunks of 4-20 s (10 s for 4K)
   at scene boundaries, frame-exactly, as lossless constant-frame-rate references.
2. Each chunk gets its own CRF from 2-3 fast probe encodes (preset +4), corrected by the measured
   VMAF gap between the probe preset and the real preset.
3. Each chunk is encoded at the real preset and measured; a chunk more than 1 VMAF off target is
   redone once with a corrected CRF. Some pictures score below the target however many bits
   they get (noise the metric reads as damage, mostly): when every probe of a chunk misses
   and the gain between them would still miss at `--min-crf` (20), the chunk is encoded at the
   highest CRF that scored within half a point of the best and not redone, and the report
   counts such scenes.
4. Chunks are joined into one AV1 stream and muxed with the source's audio, subtitles, chapters
   and metadata, at the source's picture shape and with its sound in the same place against the
   picture. Verification additionally checks the exact frame count.

On a 60 s test clip mixing six kinds of footage, `--scenes` gave a 6% smaller file at identical
full-length VMAF (93.07), with per-segment VMAF between 91.9 and 94.1 instead of 90.8 to 95.9.
On a single continuous shot it gives no gain. It took about 1.5-2x as long on short clips
and about 3x on real 15-minute recordings (six of them: 180 minutes where one pass was
estimated at 58), against a one pass that was not yet scored whole and repaired; against
one that is, it is about 1.8 times, which is what `scan` starts from; `--workers 2` encodes two chunks at once (those two then share `--memory-limit`; every other ffmpeg run has all of it).

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
video, which its four samples had not shown. Without `--scenes`, such a video gets the note
described under Search; in the usual case the check of the whole video finds the weak
stretches instead.

Finished chunks are kept in `.av1opt-work-*` in the output folder, so an interrupted run resumes
where it stopped (including the search). A chunk is taken over only if its encode has the
size and SHA-256 its record kept; any other is encoded again, and the run says how many of
each (chunks of versions that kept no hash are taken over by their size alone, and the run says how many). The folder is
gone on from only with the ffmpeg and SVT-AV1 build that made it: another build may set a
coding tool otherwise in its sequence headers, and such chunks would not join. With another
build, or with a folder of a version of av1opt that did not note the build, the file starts
over in a new folder, and the old one is removed when the new plan is made, saying why. A run
whose ffmpeg does not say in time which build it is goes on from no folder and removes none
of another build. Chunks are cut and compared at a constant frame
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
appended to `av1opt-log.tsv` there; re-running the same command resumes: a finished file is
skipped, whatever settings made it (`--overwrite` makes it again, over an earlier encode of the same file, and leaves anything else at that name alone), and files
skipped for a reason (low savings, ...) are not searched again with the same settings (target,
preset, CRF range, `--min-savings`, and `--scenes`, `--audio`, `--svt-params`,
`--tuned`, `--remove` and the samples where set) while their size and modification time stay
the same, unless `--retry-skipped` or `--force` is given. Lines from versions that did not note
the settings count only for a run at the default target and `--min-savings` and at preset 6,
the default of that time: a run at today's default looks at those videos again. A finished file counts only for the source it was made from, as the
report beside it says. A source of the same name with another size is reported, not passed
over. At the same size and another modification time the fingerprint in the report decides
(see Verify above): a source only touched, or copied without its dates, counts as done, and
one rewritten is reported. One whose report holds the time but no fingerprint cannot tell the
two apart: it is counted apart as needing confirmation, not as failed. Deleting the output
encodes the source again. A report too old to
hold the time goes by the size alone. A file there with no report, or with one that cannot
be read, says nothing about what it is: it is listed as not verified and left alone, the run
ends with status 3, and moving or deleting it has the source encoded. Only a plain file counts:
a link, a pipe or a folder at the output's name is never done, whatever report is beside it,
and the file fails with a message that says what is there.
An output is never written where its own source or another
input of the run is, whatever name leads there (`--overwrite` included): with `-o` the folder
being walked, `a.mkv` is refused when `a.mp4` is there too, and the rest of the run goes on;
with `--overwrite`, so is the second of two inputs that would write one output. An
encode from an earlier run found among the inputs is left alone. `--order per-hour` encodes the files with the largest estimated
saving per hour of encoding first (`--order savings`: the largest saving first), going by the
same estimates as `scan`, so a long run stopped early has saved the most for its time; in the
web UI files are queued in the order of the scan table. Trial encodes are remembered in `~/.cache/av1opt/trials-v2.jsonl`
for the same file (path, size, date), sample clips, settings, ffmpeg build, SVT-AV1 version and way of measuring: an encode that
was stopped or failed, or is run again with another container or `--min-savings`,
starts from the scores it already has and often measures nothing new. Another target does so
only with `--scenes`, HDR, removed sections, your own samples or a source sent to spot checks;
in the usual case the scores are counted towards a mark that depends on the target, so they
are measured again.
`--fresh-search`
measures everything again. If two inputs would produce the same output name (same file name
from two input folders), the second is reported as a name collision instead of being skipped.

### Exit status

`encode` ends with 0 when no file failed (files skipped, already done or needing confirmation are no failure), 1 on
an error that stopped the run, 2 on wrong usage, 3 when some files failed or an output already
there could not be verified and the others were dealt with, and 130 or 143 when Ctrl-C or
SIGTERM stopped it. `dups` ends with 3 when more than half of the videos could not be read: it
says so after the results, which then mean little.

A path named on the command line that does not exist or cannot be read stops `scan`, `dups`,
`bench` and `encode` with status 1 before anything is done. A folder further down that cannot be
read gets a warning, the run goes on without it, and the summary counts it.

### File names

A name that is not valid UTF-8 can't be passed to ffmpeg. `encode` refuses such a file with a
message to rename it and goes on with the others. `scan` still lists it; `dups` and `bench`
leave it out and say how many they left out. On the page the folder picker, the scan and the
duplicate search leave such names out and say how many. So do a folder that is queued, on its
job, and a benchmark. A watched folder goes on without them.

The report is named after the output and has the longer name of the two. If that name is more
than the output folder's file system takes (255 bytes on most, 143 in an encrypted home
folder), the file is refused before any sample is cut, with a message to rename it.

`av1opt-log.tsv` writes a tab, a line break or a backslash in a name as `\t`, `\n`, `\r` or
`\\`, so a name with a tab is not taken for the same name with a space.

### `encode` options

`av1opt encode <folders or files>... -o <folder>`

| Option | Default | What it does |
|---|---|---|
| `-o`, `--out-dir` | required | Folder for the finished files and their `.av1opt.json` reports; not on ZFS. |
| `--container` | `mp4` | `mp4` (plays in browsers; image subtitles dropped) or `mkv` (keeps every subtitle but TTML, and attachments). |
| `--target-vmaf` | 93 | Mean VMAF the output must reach: over the whole video where it is scored whole, over the samples where not. |
| `--preset` | 5 | SVT-AV1 preset: lower is slower and smaller. |
| `--svt-params` | `tune=0` | SVT-AV1 parameters besides the CRF, as `key=value:key=value`. |
| `--tuned` | off | Use the preset and parameters `bench --save` picked for each resolution class. |
| `--min-crf` | 20 | Lowest CRF the search tries. |
| `--max-crf` | 55 | Highest CRF the search tries. |
| `--samples` | as described under Sample | Number of sample clips the CRF search goes by (1-200). |
| `--sample-secs` | as described under Sample | Length of a sample clip in seconds (1-60). |
| `--min-savings` | 0.15 | Skip the full encode when the predicted saving of the video stream is below this (0-0.95). |
| `--force` | off | Encode even an AV1 file or one below `--min-savings`; skips noted in the log do not count. |
| `--overwrite` | off | Encode again over an output already there if its report says it was made from this same file. |
| `--retry-skipped` | off | Try again the files an earlier run skipped. |
| `--order` | `given` | Which file goes first: `given`, `savings` or `per-hour`. |
| `--keep-temp` | off | Keep the temporary folder and a per-scene work folder, for debugging. |
| `--allow-zfs` | off | Allow writing the output to ZFS. |
| `--ffmpeg` | `ffmpeg` (or `AV1OPT_FFMPEG`) | ffmpeg program to use: a build with libsvtav1 and libvmaf. |
| `--ffprobe` | `ffprobe` (or `AV1OPT_FFPROBE`) | ffprobe program to use. |
| `--scenes[=WHEN]` | `off` | A CRF per scene: `off`, `auto` (where the samples differ in quality at one CRF) or `always`; `--scenes` alone is `always`. |
| `--workers` | 1 | Chunks encoded at once with `--scenes` (1-8); changes nothing for a video encoded in one pass. |
| `--memory-limit` | `auto` | Memory cap for each ffmpeg run: `auto` (40% of RAM), `none` or `infinity`, a whole number of bytes, or a number with K, M, G, T or %. |
| `--nice` | 10 | CPU priority for encodes (0-19). |
| `--strip-metadata` | off | Drop the source's tags and chapters (languages and track names stay). |
| `--keep-dates` | off | Give each output the source's modification time. |
| `--meta-preset` | none | Add tags from a preset: `youtube`, `movie`, `tv` or `music`. |
| `--meta` | none | Set a tag, `KEY=VALUE`, with `{field}` filled from the `.info.json` or the source. Repeatable. |
| `--no-info-json` | off | Don't read yt-dlp `.info.json` files. |
| `--fresh-search` | off | Measure every CRF trial again instead of reusing those of earlier runs. |
| `--audio` | `keep` | `keep` copies AAC, Opus and MP3 and turns the rest into Opus; `shrink` also re-encodes tracks far above what Opus needs. |
| `--remove` | none | Leave a section out, `START-END`; repeatable, one input file only. |

## bench

```
av1opt bench /videos [--presets 4,5,8] [--params tune=0 --params tune=2:enable-qm=1] [--save]
```

Answers "would another preset or SVT-AV1 parameter give me smaller files, and what would it
cost in encode time?" on your own videos instead of someone else's test clips. Read-only for the
videos.

1. **Pick** – up to `--max-videos` (6) videos, spread evenly over the folders given and over
   three size classes: 720p and below, 1080p, above 1080p (by pixel count, so portrait and
   cropped videos count with their landscape peers). Dolby Vision and very short files are left out.
2. **Measure** – `--samples` (2) lossless clips of `--sample-secs` (6) seconds per video. Every
   setting (each preset with each parameter set, plus the default: preset 5 with `tune=0`) gets
   a CRF search of its own, which goes on until two neighbouring CRFs are measured (`encode`
   stops sooner), so each ends at the target VMAF; every sample encode is timed, in processor time and by the
   clock.
3. **Compare at equal quality** – a setting's size and time at exactly the target are read
   between the last CRF that passes and the first that fails. Settings are then compared with
   the default per video and averaged as ratios (geometric mean, the *size* column: the typical
   video), for all videos and per size class; *bytes* is the ratio of the sums, which counts large
   videos for more and is nearer what a library saves. A setting that fails or cannot reach the target on a video the default manages is
   shown with a note and never picked.
4. **Pick winners** – *smallest* (whatever the time), *balanced* (smallest within 10% of the
   default's encode time) and *fastest* (within 2% of the default's size). The default stays
   unless *smallest* or *balanced* gains more than 1%, or *fastest* more than 5%. A setting whose
   worst clip falls more than 0.5 VMAF below the default's is not picked.

The table lists every setting with size and time against the default, the best and worst video,
and marks the ones no other setting beats on both counts. `--csv` writes one row per video and
setting, `--json` everything including each sample encode.

`--save [balanced|smallest|fastest]` stores the winner of each size class in
`~/.local/share/av1opt/tuning.json`, with the default it beat and the target. The winner is
chosen on every other video and saved only if it also gains on the rest; a class of fewer than 3
videos goes by the winner of all videos. A class the default wins gets no entry, and loses the
one it had. `av1opt encode --tuned` then takes preset and
parameters from there by the video's size (classes without an entry use `--preset` and
`--svt-params`). An entry measured against another default than today's, or for a target more
than 1 VMAF from the encode's, is not used, and the encode says so: `av1opt bench` measures it
again.
`av1opt tuning` shows what is stored, `av1opt tuning --clear` forgets it. Without `--tuned`
nothing changes: `encode --svt-params key=value:key=value` sets parameters by hand.

Sample encodes are remembered in `~/.cache/av1opt/bench-v3.jsonl` (by file, clip layout,
encoder build, preset, parameters and CRF), so a stopped run carries on where it was and adding
a setting later only measures the new one. `--refresh` measures again.

Read the result with these in mind:

- **VMAF is the judge.** Settings tuned for metrics (`tune=1`, `tune=2`) tend to win on it;
  `tune=0` aims at how the picture looks to people, which VMAF only partly sees. Compare a clip
  by eye (the UI's Compare) before switching away from it on a few percent.
- **Times are processor time on this machine** (user and system, including reading the lossless
  clip), and settings are ranked by it; the *clock* column has the same by the clock. Other work
  disturbs the clock far more, 7 to 32% between two runs of a clip against 1 to 4.6%, but it
  disturbs processor time too: the command warns when an encode is running, and an otherwise
  idle machine is still best.
- **A dozen seconds per video is a sample.** More videos (`--max-videos`) say more than more
  settings; the per-video range in the table shows how much the videos disagree.
- Parameter names SVT-AV1 doesn't know are refused up front: ffmpeg would only warn and encode
  without them.

### `bench` options

`av1opt bench <folders or files>...`

| Option | Default | What it does |
|---|---|---|
| `--presets` | 4,5,8 | Presets to try. |
| `--params` | `tune=0`, `tune=1`, `tune=2`, `tune=0` with `qm-min=0`, `tune=0` with variance boost | An SVT-AV1 parameter set to try with every preset. Repeatable. |
| `--target-vmaf` | 93 | Mean VMAF the samples must reach under every setting. |
| `--max-videos` | 6 | Most videos used, spread over the folders and resolution classes. |
| `--samples` | 2 | Sample clips per video (1-8). |
| `--sample-secs` | 6 | Length of a sample clip in seconds (2-30). |
| `--min-crf` | 20 | Lowest CRF the search tries. |
| `--max-crf` | 55 | Highest CRF the search tries. |
| `--refresh` | off | Measure everything again instead of using remembered sample encodes. |
| `--work-dir` | `~/.cache/av1opt` | Folder for the lossless sample clips; not on ZFS. |
| `--save [PICK]` | off | Make `encode --tuned` use the winners: `balanced` (the default pick), `smallest` or `fastest`. |
| `--csv` | none | Write one row per video and setting to this CSV file. |
| `--json` | none | Write the full result to this JSON file. |
| `-j`, `--jobs` | 6 | ffprobe runs at once while the videos are collected (1-64). |
| `--ffmpeg` | `ffmpeg` (or `AV1OPT_FFMPEG`) | ffmpeg program to use: a build with libsvtav1 and libvmaf. |
| `--ffprobe` | `ffprobe` (or `AV1OPT_FFPROBE`) | ffprobe program to use. |
| `--memory-limit` | `auto` | Memory cap for each ffmpeg run, written as for `encode`. |
| `--nice` | 10 | CPU priority (0-19). |

## tuning

```
av1opt tuning            # shows the settings `encode --tuned` uses
av1opt tuning --clear    # forgets them
```

| Option | Default | What it does |
|---|---|---|
| `--clear` | off | Forget the tuned settings `bench --save` saved. |

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
  one. An original with another modification time is judged as when `encode` resumes: one only
  touched or copied is replaced, while one rewritten, one that cannot be read and one whose
  report has no fingerprint to tell by are refused, unless you say that only the date changed
  (`--accept-changed-date`, or **Replace original…** on the Compare page, which first says what
  cannot be checked). A report that holds nothing of the original but its size and date (one
  written before 2026-11-18) cannot tell it from another file of that size and date, so its
  original stays where it is whatever its date, unless you say the same. Originals that are
  links, reports without full paths (very old ones) and a new name that is already taken in the
  original's folder are refused too.
- An output with sections removed takes its original's place like any other, and the list
  says so. The removed sections are then only in the original in the trash.
- A report is a file like any other, so it is only followed where `encode` writes one: next
  to its output, under the output's name. Both files it names have to be videos. A report
  that turns up in some other folder and points at files elsewhere moves nothing.
- av1opt keeps a private record of what it made and moved, `ledger.jsonl` in its data folder
  (`~/.local/share/av1opt`, for your user only): a line for each finished encode (output, report,
  original, what tells the original from another file, a SHA-256 of the output) and for each
  replacement and undo. A report on record is followed wherever its original is, because the
  record vouches for that path. Any other report is followed only to originals inside the folders
  you name, so name the originals' folder as well or add `--anywhere`.
- Outputs that are not on record, such as those made before it began, are taken onto it by
  [`av1opt adopt`](#adopt).
- Nothing is deleted and no file is written over. The original goes to the trash of the drive
  it is on (as in the duplicates view), and the space is freed when you empty the trash. If the
  output can't be put in place after that, the original is taken back out of the trash. An
  original whose name is near the file system's limit gets a shortened name in the trash and
  still comes back under its own with `--undo`.
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
  to agree: a file goes back only if the trash's own record says it came from that place, and
  only if all of it is still what was recorded of the original (read whole against its hash, or
  against its samples where a report has only those), whatever its date says.
- av1opt's own record notes each replacement and undo before the report is rewritten. A
  report left behind (the disk was full, or the run was killed in between) is set right by
  the next `replace`, `--undo` or the page, once the record, the trash and the files (read
  whole) all show what happened; a replacement whose report could not be written ends with
  an error that says where both files are and what puts them back.
- Originals on ZFS are left alone unless `--allow-zfs` is given, and are put back there by
  `--undo` only with it.
- A later `encode` or `scan` over the library sees the replaced files as AV1 and skips them.

### `replace` options

`av1opt replace <paths>...`: output folders, the folders of the originals, reports or output files.

| Option | Default | What it does |
|---|---|---|
| `--apply` | off | Do it; without it the command only lists. |
| `--undo` | off | Take replaced originals back out of the trash and return the AV1 files (acts only with `--apply`). |
| `--allow-zfs` | off | Also replace originals stored on ZFS. |
| `--accept-changed-date` | off | Take an original whose date changed as the file that was encoded, where its report cannot tell; originals seen to differ are still refused. |
| `--anywhere` | off | Follow reports to originals wherever they are, not only in the folders named. |

## adopt

```
av1opt adopt ~/av1opt-out            # lists what it would add to the record
av1opt adopt ~/av1opt-out --apply
```

Takes outputs that are not on av1opt's private record (see [replace](#replace)), such as those
made before it kept one, onto it, so that the page can replace them and put their originals
back. It reads the reports under the folders named, lists what it would add, and writes
nothing without `--apply`, and then only to the record; nothing is adopted without this
command. Naming a folder here says that you trust the reports in it, as naming it to `replace`
does. As with `replace`, a report is followed only to originals inside the folders you name
(or anywhere with `--anywhere`). A report has to hold the original's date and a fingerprint or
hash of it, which reports written before 2026-11-18 lack (`replace --accept-changed-date` still
replaces those), and a report whose output or original fails the checks `replace` makes is
refused. An output that already replaced its original is taken too if it is where `replace`
puts it, in the original's place, and the file in the trash says it came from there and is,
read in full, the one the report describes.

| Option | Default | What it does |
|---|---|---|
| `--apply` | off | Write to the record; without it the command only lists. |
| `--anywhere` | off | Follow reports to originals wherever they are, not only in the folders named. |

## ui (optional)

```
cargo build --release --features ui
av1opt ui [--port N] [--no-open] [--memory-limit 12G]
```

A local web app in the browser (Chrome or Firefox), built from the same scan and encode code:

- **Scan**: pick folders, see per-file and total savings estimates, select candidates and queue them.
  The page's folder picker (the desktop app uses the system's) has a path field at the top:
  type a folder and press Enter to go there; a path it cannot list is said under the field.
  **Time** is the estimated search and encode time of a file and **Per hour** its saving per
  hour of encoding (click to sort); the selection line adds up both for the ticked files.
  **Most saved in N h → Pick** ticks the candidates that save the most together in that many
  hours of encoding (`scan --hours`); it starts at the length of the encoding hours when
  those are set.
- **Duplicates**: the `dups` search with progress and a Stop button. Each group shows a frame
  of every copy, the suggested keep and why; **Compare** opens two copies in the split player.
  Tick the copies you don't want, then **Move to trash** (after a confirmation), or copy or save
  the list of paths. Nothing is deleted: each file is renamed into the trash of the drive it is
  on (`~/.local/share/Trash`, or at the top of another drive `.Trash/<uid>` or `.Trash-<uid>`,
  as file managers do), so it can be put back until you empty the trash, and no space is freed
  before that. The last copy of a video is never trashed, and nothing is trashed in favour of
  a copy that itself lies in a trash. Just before each move the file and the copy that stays are looked at
  again: if either is gone or has changed since the search (size, time or sampled content), that
  file is refused, and a move that turns out to have taken the kept copy along is undone at once.
  A trash folder is used only if it and its `files` and `info` are real folders of yours that
  nobody else can write to (`.Trash` must be sticky, otherwise `.Trash-<uid>` is used). A drive
  without a usable trash gives an error instead of a copy to another disk.
- **Encode dialog**: target VMAF, preset, container, encoder parameters, metadata, sections
  to remove, and **Shrink heavy sound tracks to Opus** (`--audio shrink`, off by default).
  It opens with the settings of the last encode queued, and the scan's times go by its
  preset: a preset chosen once stays, and a new default shows only where nothing was queued
  yet. Its number fields take the ranges of the command line and say which field is out of
  range, and why, before anything is queued.
- **Queue**: encodes run one at a time with live progress: CRF search trials, chosen CRF,
  the estimated time left per job and for the whole queue (files no scan has seen are
  estimated once they are probed),
  per-chunk results in scene mode, percent, fps and ETA. Cancel stops ffmpeg at once; finished
  scene chunks are kept, so queueing the same files again resumes. Unfinished jobs come
  back on their own the next time `av1opt ui` starts, once the page is up, and the Queue page
  says how many are still to come. **Do next** on a waiting job
  starts it before the other waiting ones, and **Queue again** on a cancelled or failed job
  queues its files once more with the same settings. Inputs of a saved or requeued job that
  are gone are left out and named on the job. They stay saved when the job finishes, fails or
  is taken off the list, and are queued once they are back at a later start; a saved job with
  no input left is listed apart, with **Try again** and **Forget** on each entry and for the
  whole list. The **Try again** of the list leaves out videos of jobs you cancelled or removed:
  those start only from their own entry.
  A job queued by hand makes its output folder when it is queued. One that comes back from
  the saved queue, is queued again or comes from a watch needs that folder to exist, so that
  nothing is written under the mount point of a drive that is away: without it the job fails
  saying so and stays in the saved queue for the next start, until you remove it (**Clear
  finished** leaves it). Such a job also checks the folder before each file, so one whose
  drive is lost on the way waits in the same manner. A folder also counts as away when it is on
  another drive than before, when it is at or under a mount point `/etc/fstab` lists with nothing
  mounted on it, and when it is empty on the same drive as the folder above it with nothing
  saying it is the right one, since an unmounted drive's mount point looks like that.
  **Queue again** on such a job, or **Use this folder** on a watch, says the folder is the right
  one: it asks first if a drive may be missing, notes where the folder is now, and makes a missing
  folder when the folder above it is in use. Only a drive that `/etc/fstab` expects cannot be
  overruled this way. Jobs and watches saved by earlier versions get their folder made once.
  **Pause** holds the queue until
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
  download still running is left alone, and only if a scan would call it a candidate or say it
  may save too little: AV1, Dolby Vision, too short, too little to gain and unreadable files
  are left. A watch with
  **Force** ticked leaves only AV1 and unreadable files. A video whose probe could not
  finish, or that the system could not read, is not called unreadable at once: it gets three
  looks and is then tried at growing intervals, up to once a day. Until a probe gets through
  it is listed as not read yet, with the reason and the number of tries, and **Look now** tries
  it at once.
  One that an earlier version had given up on is tried again. The videos in the folder
  when it was added are not queued unless asked for. Queueing is all a watch does: the
  pause and the hours decide when the encode runs, and originals are never replaced or
  touched. The output folder is made when the watch is added, and while it is not there
  videos that are ready wait for it. It may lie inside the watched folder, which is then
  walked without it, but it may not be that folder or contain it, wherever links lead; a
  watch saved earlier with such a folder stays listed, says so and is not walked. What was
  seen is kept in `ui-watch.json`. A video gone from a folder that still holds other videos
  is forgotten after a month, and a look that finds no videos at all forgets nothing, so a
  drive that was not mounted does not come back as a folder full of new videos.
- **Compare**: source and AV1 output on top of each other with a split slider, synced playback,
  frame stepping and zoom, plus the per-chunk VMAF timeline from the `.av1opt.json` report
  (click to seek). For a one-CRF encode the timeline shows the stretches the whole-video check
  had encoded again, and the CRF row says the CRF of the file as kept: the stretches' CRFs, the
  first CRF where the whole video was encoded a second time, and where the check encoded all
  of it again, that it did. History's CRF column says the same in short. Play waits until the
  source has loaded beside the output (or has failed to, when the output plays alone), and
  for nothing else: the two are lined up meanwhile on copies of their own that are not on
  the page, read in a worker. An encode's output moves at most three frames from where its
  report puts it, and only for a clear match; two duplicates, whose offset nobody knows, are
  searched across the whole left copy for about 4 s of work (15 s at most), each better offset
  used as soon as it is found, also while playing. Sources the browser can't decode (HEVC in some browsers, MPEG-2, ...) are shown as a
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
  `replace` above), all ticked but those whose output leaves sections of the original out
  (marked, and said above the list): those are replaced only when ticked one by one, and
  **All / none** leaves them alone. It works through the ticked ones one by one and keeps the ones
  that failed on screen with the reason. Replaced encodes carry a "replaced" mark. An original
  that is still in the encode queue is refused. The page acts only on outputs in av1opt's private
  record (see `replace`). Others are marked "not made here" until `av1opt adopt` has been run on
  their folder in a terminal.

What the page remembers of its forms is kept by the server in `ui.json`, not in the browser:
the browser keeps such things per address, and the port, so the address, is new at every
start. That is the encode dialog's settings and output folder, the values of the scan,
duplicate and benchmark forms (each number as soon as its field changes to one the field takes,
not only when a run starts), their folders, filters and tabs, the folder the picker was in,
and which start-up notices were dismissed (a notice about a file damaged again comes back).
Every browser and desktop window sees the same values. Two tabs that change the same value
leave the later one, and neither takes the other's away. What is kept is a remembered form
value and no more: the server keeps at most 64 values of 16 KiB each, 64 KiB in all, and
folder lists of absolute paths; the page puts a value back only into a field of its kind that
takes it (a number within the field's range, a choice among its options, a benchmark preset
that has a box), and a field given none starts at its default.

The server listens on 127.0.0.1 only, on a free port unless `--port` names one, and prints its
address and a sign-in link with a random token (the browser
gets it as a cookie). Requests with a foreign Host or Origin are refused, so are requests from
a program of another user of the machine, or from a connection the system no longer shows as
anyone's (a browser sends the cookie to every port of the host, so the cookie alone would not
keep them out). The system's own programs (root) pass while their connection is open, and where
the system's list of connections can't be read the cookie alone decides. Past 64 connections
open at once, the server closes the extra ones: another user's first, then the ones that have
not sent a whole request, oldest first, then idle ones that have, quietest first, and the
page's own event stream, videos and pictures only when nothing else is left. It raises its open-file limit to the most the system allows,
and when a connection cannot be taken (no file left, say) it takes new ones again a moment
later instead of stopping. Media are only
served for the video files the page already lists: those you picked, scanned, found as
duplicates or queued, and the outputs and originals of its jobs and reports. Output folders on ZFS are refused (no override in the UI);
replacing an original that is on ZFS needs a tick in the dialog, or the confirmation in Compare, which says so.
Putting one back there, and moving duplicates on ZFS to the trash, ask in a dialog of their own.
The ffmpeg and ffprobe programs set in Settings have to be named like that (`ffmpeg`, `/opt/x/ffprobe-7`),
be there, and be yours or the system's, writable by nobody else, in folders of which the same holds.
What is kept is the file the path leads to, links followed, and that file has to be named like that too.
`AV1OPT_UI_TOKEN` sets the token (for a desktop wrapper loading the same page);
`AV1OPT_UI_DEV=<repo>/ui` serves the page files from disk while working on them.

| Option | Default | What it does |
|---|---|---|
| `--port` | 0 | Port to listen on; 0 picks a free one. |
| `--no-open` | off | Don't open a browser; only print the sign-in link. |
| `--ffmpeg` | `ffmpeg` (or `AV1OPT_FFMPEG`) | ffmpeg program to use, unless Settings name another. |
| `--ffprobe` | `ffprobe` (or `AV1OPT_FFPROBE`) | ffprobe program to use, unless Settings name another. |
| `--memory-limit` | `auto` | Memory cap for each ffmpeg run, written as for `encode`. |
| `--nice` | 10 | CPU priority for encodes (0-19). |
| `-j`, `--jobs` | 6 | ffprobe runs at once for scans (1-64). |

**Settings** sets the ffmpeg and ffprobe programs (saved in `~/.local/share/av1opt/ui.json`,
winning over `--ffmpeg` / `AV1OPT_FFMPEG`) and checks them for libsvtav1 and libvmaf. While they
lack either, a banner says so and queued jobs wait instead of failing. `av1opt encode` runs the
same check before it starts.

One `av1opt ui` runs per user, or more exactly per data folder (`~/.local/share/av1opt`, where
it holds a lock on `ui.lock`): two would encode the same saved jobs and overwrite each other's
queue. A second start is refused and names the process that is running.

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
the background. The page may copy to the clipboard and show a video full screen; every other
permission is denied. The server's messages are kept, with times, in
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
`../target/release/av1opt`, then `av1opt` on PATH. An installed app therefore goes on running
the av1opt packed into it, whatever has been built since, until a new package is made and
installed. Extra arguments go in `AV1OPT_UI_ARGS`
(for example `AV1OPT_UI_ARGS="--memory-limit 12G"`). A desktop launcher has no shell
environment, so set the ffmpeg path once in Settings.

### Safety

- `encode` never modifies, moves or deletes an input. Only `replace` (and the two buttons for
  it in the UI) moves originals, and only to the trash.
- Refuses to write to ZFS unless `--allow-zfs` is passed.
- An output file that appears in the output folder while its encode runs is not written
  over: the finished encode is kept next to it under another name, and the run says where.
- What av1opt reads in the output folder (`av1opt-log.tsv`, the reports, a work folder's plan)
  is read only if it is a plain file of a sensible size (8 MiB for a report, 128 MiB for the
  log, 64 MiB for a plan); a pipe, a link or a larger file there is passed over with a warning, never waited on.
- Each output is locked while it is made (`.av1opt-lock-<number>` in the output folder). Something
  at that name that is not a plain file of yours stops that file with a message: move it away
  if no av1opt is working there. When the filesystem gives no lock for any other reason (it has
  none, or a network drive fails), the run goes on, with a warning.
- Each ffmpeg run gets a memory cap (`--memory-limit`: `12G`, `1.5G`, `800M` or `40%`, shared
  by the chunks `--workers` runs at the same time, or `none` for no cap; default 40% of RAM) and low CPU priority (`--nice 10`), so a 4K encode
  can't freeze the desktop.
- Temp files live in a hidden folder in the output directory and are removed afterwards; leftovers
  from crashed runs are cleaned up on the next run.
- The temporary folder and a scene-mode work folder are made private to you (mode 0700) and held
  open from the moment they are made: everything in them, by av1opt, ffmpeg and ffprobe alike, is
  reached through `/proc/<av1opt's process number>/fd/<number>`, never by their name. In an
  output folder others can write to, someone who renames one and puts a link at its name changes
  nothing: the encode is still made, hashed and moved in av1opt's own folder, and nothing in the
  folder the link leads to is written or removed. The encode's SHA-256 is taken before it is put
  in the output folder, and if what has its name there a moment later is another file, nothing
  is recorded. Without `/proc`, or with ffmpeg run where it cannot see av1opt's process (a
  sandbox of its own), the folders are reached by their names, and an output folder that others
  can write to needs the sticky bit (`chmod +t`), or the file is refused; one that only your own
  group (the one named after you, with nobody else in it) can write to counts as yours. On a
  drive that keeps no modes or owners (FAT, exFAT, NTFS, some network drives) nothing can be
  private: the folders are used by their names, and the run says so once.
- Output that nobody reads stops no work. When standard error is gone (a `tee` that has ended, a
  terminal closed under a program that outlives it) messages are dropped and the work goes
  on. When standard output is gone, as in `av1opt scan /videos | head`, the command ends
  quietly, like any other in a pipe.

### Saved files and versions

Reports, history lines, `ui.json`, `ui-queue.json`, `ui-watch.json`, `tuning.json`, benchmark results and the scene-mode work folders carry a
`format` number. Files from before versioning count as format 1. A newer av1opt keeps reading
older files; an older one skips reports and history lines from a newer format, leaves a newer
`ui.json` untouched, sets a newer `ui-queue.json` or `ui-watch.json` aside as
`ui-queue.format<N>.json` (the videos in a watched folder then count as there before), and starts
a scene-mode file over rather than resuming a work folder of another format. A `tuning.json`
from a newer av1opt is left as it is: `encode --tuned` refuses it and the page does not show it.

A `ui.json` or `ui-queue.json` that can't be read is not written over. It is kept next to where
it was under a name with the time in it (`ui.unreadable-20261118-213005.json`), and the server's
output and a banner on the page say so. av1opt then starts with default settings or an empty
queue. Settings hold the pause, the hours and the watched folders, so after losing them the
queue starts paused until you resume it. A damaged line in `history.jsonl`, the trial cache,
`av1opt-log.tsv` or the benchmark's saved measurements costs that line alone, and a last line
cut short does not take the next one with it. A failed save of the queue or of the last
benchmark is said once, until saving works again.

### Debugging

`AV1OPT_DEBUG=1` prints every ffmpeg command; files in the temporary and work folders appear
in them as `/proc/<number>/fd/<number>/...` (see Safety). `--keep-temp` keeps the temporary folder in the
output folder (`.av1opt-tmp-<number>`, until a later run there clears it), with the partial
encode if the encode fails, and the work folder of a per-scene encode. Sample clips are deleted
all the same once the search is done with them.
`cargo test --features ui` also runs `tests/ui_api.rs`, which starts the real `av1opt ui` and
checks sign-in, request checks, media access, the job queue, its pause and hours, the duplicate search (and what its
trash action refuses), replacing an original and putting it back (and which reports may do that)
and the benchmark's requests and tuned settings over HTTP (no ffmpeg needed).

## Environment

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
  trials led to the same CRF. So the worst sample was made to count: past what verification
  allowed, the CRF came down before the encode, and a video with samples far apart could be
  encoded per scene (`--scenes=auto`). Since the whole video is scored after its encode (see
  below), the CRF comes down this way only in HDR and with sections removed.
- **A Matroska header names a frame rate whatever the frames do.** ffprobe reports 24 fps as
  both the nominal and the average rate of an MKV whose frames come at 12, so comparing the
  two finds variable frame rates in MP4 only. Scene mode then cut such a file into
  constant-rate chunks: 96 frames became 190, every other one a repeat (a file with more
  frames than its header says would have lost some), and the checks passed because they
  compare at the same constant rate. ffmpeg's `fps` filter counts the frames it drops and
  repeats; scene detection runs it over the whole file anyway, so that count now decides.
  Everything else still went by the header: a file declared at 48 fps whose frames come at
  24 had its key frames 20 s apart, and one declared at 30 with a frame every half second
  failed as broken, because the check of a stretch put back into the video cut it on a
  grid of 30 fps, each frame there 15 times, and VMAF reads frames that do not move lower
  (96.1, where the same frames one each read 100). Where the two rates agree, the probe of a
  Matroska file now reads its first 32 packets, and its middle 32 where those are slower,
  and takes the rate they show where it is below the declared one in both places: what the
  same frames in MP4 say (24.0 and 2.0). A frame held over four times as long as the
  others, such as a still first frame, does not count, and a slow title before a steady
  film keeps the declared rate.
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
  averages the hard moment away. So samples became 4 seconds, 8 of them, and more with
  length (today only where one CRF has to carry its worst stretch; the usual search has since
  moved to clips of 1 second, see below). The worst of eight short samples lies further below their mean than the worst of
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
- **Samples miss the mean by where they lie, not by how long they are.** Scoring the whole
  video shows what the search could not: at the CRF eight clips of 4 seconds chose for a
  mean of 93, three real videos came out at 93.8, 94.0 and 90.7 over their whole length.
  Taken apart on those videos (every frame of a whole encode scored, the clips' own frames
  set against the rest), a clip's score is within 0.2 of the same frames in the whole
  encode at any length down to one second; the error is all in which seconds were looked
  at. Eight clips of 4 seconds read 2.5 high, 0.9 low and 0.6 low; 64 of one second, for
  the same seconds of encoding, 0.5 low, 0.2 high and 0.1 high, and 0.26 off (root mean
  square) wherever the grid happens to start, where the eight were 0.5 to 1.3 off. Skipping
  the first and last 5% read 0.1 to 0.4 high and was dropped. Correcting the clips' mean by
  how many bits the source spends near each one did nothing: a source encoded at a flat
  rate says nothing about where AV1 will have it hard.
- **The stretches encoded again lift the mean, so the search counts on them.** With the
  mean found that closely, a video with a hard stretch still ended well above its target:
  aimed at 93, one 15-minute video scored 93.8 after its one pass and 94.6 once 4:28 of it
  had been encoded again, at 196 MB. A search that counts every sample below the line as
  1.5 above it (repairs imitated on the per-frame scores of three videos said that is
  about where they end, a little short where much is lifted) picks the CRF whose mean
  reaches the target after the repairs: the same video 184.7 MB at a mean of 93.8 (CRF
  38.75 instead of 37; 92.5 before 5:52 of it was encoded again), and a second 373.3 MB
  at 93.2 instead of 429.7 MB at 94.2 (CRF 39 instead of 37, 1:56 encoded again instead
  of 1:05), and a third 205.1 MB at 93.2 instead of 220.1 MB at 94.1. The three together
  are a tenth smaller, and their lowest five seconds stayed at 89.1 to 89.5.
- **A key frame every 10 seconds, and quantization matrices.** SVT-AV1 puts a key frame
  about every 5 seconds and leaves quantization matrices off. At equal VMAF, 10 seconds
  took 3.3% less on eight 10-second clips and 2.6% less on four of 40 seconds (20 seconds:
  3.2%, none at all: 3.8%, so most of the gain is in the first doubling, which keeps
  seeking quick and the stretches that can be encoded again short). Matrices took another
  1.1%, less on every clip. Both together: 4.4% on the eight clips they were chosen on and
  2.1% on six they were not (less on each of the six, by VMAF NEG and SSIMULACRA2 too),
  with encoding time within 2% either way. Tried the same way and left out: `tune=2`, variance
  boost, encoding at a reduced resolution, film grain synthesis (3 to 8% larger at equal
  scores, which is all these metrics can say about it), and `tune=1` (better on some clips,
  worse on others).
- **`tune=5` wins by VMAF and by nothing else.** SVT-AV1 4.2's VMAF tune sharpens the
  picture before encoding it. At equal VMAF it is 21 to 33% smaller on three sets of clips;
  at equal VMAF NEG (which takes enhancement out) 6 to 12%; at equal SSIMULACRA2 0 to 4%
  *larger*, at equal PSNR-HVS 2.5% larger, at equal MS-SSIM 2.3% smaller. At the same VMAF
  its NEG is 1.1 to 1.5 lower and its SSIMULACRA2 4 to 5 lower: most of the gain is the
  sharpening VMAF rewards. It is not the default; `--svt-params tune=5` has it.
- **A slower preset is the one sure gain left, and it costs time.** Against preset 6 at
  equal VMAF: preset 5 is 6.0%, 5.5% and 4.9% smaller on three sets of clips (eight of 10
  seconds, six others, four of 40 seconds), preset 4 9.1%, 9.7% and 8.1%, on every clip and
  by VMAF NEG, XPSNR and SSIMULACRA2 alike, for 28% and about 60% more processor time in
  the encode. One real 15-minute video through the whole of `encode` at preset 5: 191.3 MB
  at a mean of 93.1 instead of 205.1 MB at 93.2, the lowest five seconds at 89.0 and 89.1,
  in 54 minutes instead of 52 (search, check and repairs take the same time at either; the
  machine was busy both times). The default went from 6 to 5 on that; `--preset 6` gets
  the quicker encode back. Preset 4 was the default for a morning (2026-11-19) and went
  back: against 5 the clips say 3.3%, 4.4% and 3.4% smaller for 22 to 29% more processor
  time in the encode, but two whole 15-minute videos through `encode` came out only 1.6%
  and 1.0% smaller (108.9 MB against 110.6, 77.0 against 77.8, at the same mean and lowest
  five seconds) for 23% and 20% more time. A quarter of each was encoded again by the
  repairs at either preset, and both needed a CRF above 42; why whole videos gain a third
  of what clips do is not known. `--preset 4` is there for a library that can wait, and
  `--preset 3` further still: 16.0% smaller than 6 on the first set of clips (8 to 26% by
  clip; 14 to 16% by the other three metrics) for three times the processor time.
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
trial cache (what counts as the same file, clips and settings), the mean that counts on
weak stretches being encoded again, what is done with a finished file by its size, which sound tracks are copied
or shrunk, chunk CRF
estimation, scene chunk planning, sample placement, frame-rate snapping and source filters, plus
the duplicate finder's hashes (scaling, noise, black bars, flat frames), pair rules and keep
suggestions, and the benchmark's arithmetic (size and time at the target, comparison across
videos, picks, spreading the test videos), the pick for a number of hours (against trying
every subset, and on 30,000 videos), what a watched folder makes of the videos it finds, and `replace`: the round trip with `--undo`, an
output with the original's own extension, an output on another drive, every refusal (and that a
refusal moves nothing), a trash that fails, a name taken at the last moment, two runs on one
report at once.
