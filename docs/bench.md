# bench

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

## `bench` options

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
