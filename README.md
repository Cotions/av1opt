# av1opt

Shrink video libraries by re-encoding to AV1 at a target perceptual quality (VMAF),
instead of one fixed setting for everything. Built on stock FFmpeg with SVT-AV1 and libvmaf.

Two commands:

- `scan` — read-only survey of a library: what each file is, what re-encoding would roughly save.
- `encode` — per-file CRF search against a VMAF target, full encode, verification, report.

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

## encode

```
av1opt encode movie.mkv some/folder -o /out [--target-vmaf 93] [--preset 6] [--container mp4|mkv]
```

For each file:

1. **Sample** – cuts up to 4 clips spread through the video into lossless, constant-frame-rate
   references (deinterlaced and cropped to even size when needed).
2. **Search** – finds the highest CRF whose mean VMAF over the samples meets the target. It
   interpolates between passing and failing CRFs, averaging ~3.4 trial encodes, then refines to
   SVT-AV1's quarter-step CRF. 4K uses the `vmaf_4k` model.
3. **Decide** – skips files that are already AV1, Dolby Vision, can't reach the target, or would
   save less than `--min-savings` (15%). HDR10 and HLG are encoded with their colour signalling
   and mastering/light-level metadata intact (VMAF is then measured on the HDR signal, which
   tracks encoding damage but is not calibrated like SDR scores).
4. **Encode** – SVT-AV1, 10-bit. Audio already in AAC/Opus/MP3 is copied, anything else becomes
   Opus. MP4 output keeps text subtitles (as mov_text) and plays in Chrome and Firefox; use
   `--container mkv` to keep image subtitles too. Cover art is kept (MP4: attached picture,
   MKV: attachment with its original file name).
5. **Verify** – codec, duration, stream and cover counts, HDR transfer, plus VMAF spot checks at positions the search
   did not sample. Nothing is written unless all checks pass.
6. **Report** – the output plus `<name>.av1opt.json` with every CRF tried, scores, sizes and timings.

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
`--retry-skipped` is given. If two inputs would produce the same output name (same file name
from two input folders), the second is reported as a name collision instead of being skipped.

## ui (optional)

```
cargo build --release --features ui
av1opt ui [--port 8765] [--no-open] [--memory-limit 12G]
```

A local web app in the browser (Chrome or Firefox), built from the same scan and encode code:

- **Scan**: pick folders, see per-file and total savings estimates, select candidates and queue them.
- **Queue**: encodes run one at a time with live progress: CRF search trials, chosen CRF,
  per-chunk results in scene mode, percent, fps and ETA. Cancel stops ffmpeg at once; finished
  scene chunks are kept, so queueing the same files again resumes. Unfinished jobs are
  re-queued automatically the next time `av1opt ui` starts.
- **Compare**: source and AV1 output on top of each other with a split slider, synced playback,
  frame stepping and zoom, plus the per-chunk VMAF timeline from the `.av1opt.json` report
  (click to seek). Sources the browser can't decode (HEVC in some browsers, MPEG-2, ...) are shown as a
  20 s near-lossless H.264 preview made on demand in `~/.cache/av1opt/previews`, never next to
  the source; MKV outputs the browser won't open are remuxed to MP4 there too.
- **History**: every report found in the output folders used so far (and folders you add).

The server listens on 127.0.0.1 only and prints a sign-in link with a random token (the browser
gets it as a cookie). Requests with a foreign Host or Origin are refused, and media are only
served for files you picked in the app. Output folders on ZFS are refused (no override in the UI).
`AV1OPT_UI_TOKEN` sets the token (for a desktop wrapper loading the same page);
`AV1OPT_UI_DEV=<repo>/ui` serves the page files from disk while working on them.

**Settings** sets the ffmpeg and ffprobe programs (saved in `~/.local/share/av1opt/ui.json`,
winning over `--ffmpeg` / `AV1OPT_FFMPEG`) and checks them for libsvtav1 and libvmaf. While they
lack either, a banner says so and queued jobs wait instead of failing. `av1opt encode` runs the
same check before it starts.

Stopping the server (Ctrl-C, SIGTERM) stops running ffmpeg processes and keeps unfinished jobs
for the next start.

### Desktop app (optional)

`desktop/` wraps the same page in an Electron window. Its bundled Chromium always plays AV1 and
H.264, whatever browser is installed. It starts `av1opt ui` with a random token, shows the page,
and stops the server when the window closes. If the window process dies, the server notices
(stdin closes) and stops too. Closing the window while an encode runs asks first. Folder
choices use the system dialog, and a notification says when a job ends while the window is in
the background.

```
cargo build --release --features ui
cd desktop
npm install          # if npm skips electron's download script: node node_modules/electron/install.js
npm start            # runs ../target/release/av1opt
npm run dist         # dist/av1opt-<version>.AppImage with the av1opt binary inside
npm run install-desktop   # copies it to ~/.local/opt/av1opt and adds av1opt to the app menu
```

The app uses, in order: `$AV1OPT_BIN`, the binary packed into the AppImage,
`../target/release/av1opt`, then `av1opt` on PATH. Extra arguments go in `AV1OPT_UI_ARGS`
(for example `AV1OPT_UI_ARGS="--memory-limit 12G"`). A desktop launcher has no shell
environment, so set the ffmpeg path once in Settings.

### Safety

- Inputs are never modified, moved or deleted.
- Refuses to write to ZFS unless `--allow-zfs` is passed.
- Each ffmpeg run gets a memory cap (`--memory-limit`, default 40% of RAM) and low CPU priority
  (`--nice 10`), so a 4K encode can't freeze the desktop.
- Temp files live in a hidden folder in the output directory and are removed afterwards; leftovers
  from crashed runs are cleaned up on the next run.

### Debugging

`AV1OPT_DEBUG=1` prints every ffmpeg command. `--keep-temp` keeps samples and the partial encode.

## Lessons baked in

- **Compare frames by index, not timestamp.** Encoders and muxers shift start times; libvmaf's
  timestamp sync then compares neighbouring frames and reports nonsense (60-75 instead of 98).
- **Snap to the closest standard frame rate.** Treating 24 fps as 23.976 drops a frame every ~42
  and misaligns comparisons.
- **Pre-roll before cutting.** Seeking straight into MPEG-2/TS or open-GOP H.264 can return frames
  from the wrong place; seek 2 s early and discard the warm-up after decoding.
- **Score every 3rd frame, not every 2nd.** SVT-AV1's hierarchical GOP alternates better and
  worse frames; an even VMAF stride measures only one kind (+0.2 VMAF bias).
- **CRF has quarter steps.** `-svtav1-params crf=35.25` works; integer CRF alone left every
  chunk ~0.3 VMAF above target.
- **Cover art can be a plain video stream.** Pick the real video by codec, not "first video stream".

## Tests

`cargo test` covers the CRF search (exactness against brute force, trial count), chunk CRF
estimation, scene chunk planning, sample placement, frame-rate snapping and source filters.
