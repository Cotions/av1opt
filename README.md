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
*candidate* or a skip reason (already AV1, HDR, too short, low expected gain), and prints totals
by codec and folder plus the biggest wins. Savings are a rough metadata-based estimate shown as a
range; `encode` measures the real numbers. Never writes to the scanned folders.

## encode

```
av1opt encode movie.mkv some/folder -o /out [--target-vmaf 93] [--preset 6] [--container mp4|mkv]
```

For each file:

1. **Sample** – cuts up to 4 clips spread through the video into lossless, constant-frame-rate
   references (deinterlaced and cropped to even size when needed).
2. **Search** – finds the highest CRF whose mean VMAF over the samples meets the target. It
   interpolates between passing and failing CRFs, averaging ~3.4 trial encodes. 4K uses the
   `vmaf_4k` model.
3. **Decide** – skips files that are already AV1, HDR, can't reach the target, or would save less
   than `--min-savings` (15%).
4. **Encode** – SVT-AV1, 10-bit. Audio already in AAC/Opus/MP3 is copied, anything else becomes
   Opus. MP4 output keeps text subtitles (as mov_text) and plays in Chrome and Firefox; use
   `--container mkv` to keep image subtitles too.
5. **Verify** – codec, duration and stream counts, plus VMAF spot checks at positions the search
   did not sample. Nothing is written unless all checks pass.
6. **Report** – the output plus `<name>.av1opt.json` with every CRF tried, scores, sizes and timings.

Folders are walked recursively and their layout is mirrored under `--out-dir`. Every result is
appended to `av1opt-log.tsv` there; re-running the same command resumes: finished files are
skipped, and files skipped for a reason (low savings, ...) are not searched again unless
`--retry-skipped` is given.

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
- **Cover art can be a plain video stream.** Pick the real video by codec, not "first video stream".

## Tests

`cargo test` covers the CRF search (exactness against brute force, trial count), sample placement,
frame-rate snapping and source filters.
