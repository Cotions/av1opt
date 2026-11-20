# av1opt

Shrinks a video library by re-encoding it to AV1, with the quality chosen per video
instead of one fixed setting for everything.

For each video it looks for the smallest file that still scores 93 on VMAF (a 0 to 100
measure of how close an encode looks to its source; 93 is hard to tell apart). It then
checks the whole result against the original and encodes weak stretches again. A video
that would not come out at least 15% smaller is left alone. Originals are never touched
until you say so.

The output is plain AV1 in MP4 or MKV and plays in Chrome, Firefox and anything else that
plays AV1. It is built on stock FFmpeg with SVT-AV1 and libvmaf. Linux only so far.

## Install

You need:

- FFmpeg built with `libsvtav1` and `libvmaf`, for example a
  [BtbN static build](https://github.com/BtbN/FFmpeg-Builds). If it is not the `ffmpeg` on
  your path, point at it with `AV1OPT_FFMPEG` and `AV1OPT_FFPROBE`.
- Rust, to build.
- Optional: `systemd-run --user`, which lets av1opt cap the memory of each encode.

```
cargo build --release                 # command line only
cargo build --release --features ui   # with the web app
```

The program is `target/release/av1opt`.

## Use

```
av1opt scan /videos                   # what is there, and roughly what encoding would save
av1opt encode /videos -o /out         # encode into /out; /videos is not changed
av1opt replace /out                   # list what would take its original's place
av1opt replace /out --apply           # do it; originals go to the trash
```

Or all of it in a browser page, with a queue, side-by-side comparison of original and
result, and watched folders:

```
av1opt ui
```

Encoding is slow by design: expect about one and a half times a 1080p video's length on a
six-core machine. `scan --hours 8` picks the videos that save the most in a night.

## Commands

| Command | What it does | Details |
|---|---|---|
| `scan` | Read-only survey: what each file is and what it would save. | [docs/scan.md](docs/scan.md) |
| `dups` | Read-only search for videos kept more than once. | [docs/dups.md](docs/dups.md) |
| `encode` | Finds the setting for the target, encodes, checks, writes a report. | [docs/encode.md](docs/encode.md) |
| `replace` | Puts outputs in place of their originals; `--undo` puts them back. | [docs/replace.md](docs/replace.md) |
| `adopt` | Takes outputs made before av1opt kept a record onto the record. | [docs/adopt.md](docs/adopt.md) |
| `bench` | Measures which encoder settings pay off on your own videos. | [docs/bench.md](docs/bench.md) |
| `tuning` | Shows or forgets what `bench --save` stored. | [docs/tuning.md](docs/tuning.md) |
| `ui` | The web app, and the desktop app in `desktop/`. | [docs/ui.md](docs/ui.md) |

`av1opt <command> --help` lists every option.

## More

- [Environment variables](docs/environment.md)
- [Lessons baked in](docs/lessons.md): what went wrong along the way and what the code does about it
- [Tests](docs/tests.md)

## Licence

MIT; see [LICENSE](LICENSE).
