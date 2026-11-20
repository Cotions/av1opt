# dups

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

## `dups` options

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
