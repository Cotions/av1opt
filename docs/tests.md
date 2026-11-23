# Tests

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
left behind, no ffmpeg left running, trials remembered), an encode killed at each step of finishing it (the output moved out, its report, the record, the history, the log) and run again, `--scenes=auto` on an even and an
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
report at once, and a replacement or an undo stopped between each two of its moves (on one drive and across two) and then run again either way, and `adopt --apply` stopped between two outputs.
