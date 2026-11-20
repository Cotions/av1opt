# A "slow, smallest" mode: spend much more time for a smaller file at the same quality

Status: open, not started. Opened 2026-11-20 at the owner's request.

## What is asked

An option, per file or per batch, that trades encoding time for size: the same VMAF target and
the same checks, a file as small as the machine can make it, for videos that matter enough to
wait for. The default stays as it is (SVT-AV1 preset 5).

## What was measured

On the test rig's clips (`~/av1opt-test/efficiency/README.txt`), size at equal VMAF, against
SVT-AV1 at the default of the day:

| Encoder and setting | Size | Processor time | Notes |
|---|---|---|---|
| SVT-AV1 preset 4 | -3.3 to -4.4% | +22 to 29% | Whole runs of av1opt gave only -1.0 and -1.6% |
| SVT-AV1 preset 3 | -16.0% (-25.5 to -8.1 by clip) | x3 | Train clips only, never validated on the other sets |
| libaom, `cpu-used 4` | -21.8% (-26.9 to -11.7) | x12 | Train clips, against preset 6; other metrics agree (-17 to -22%) |

Two cautions on these figures:

- **Clips overstate what a whole run gains.** For preset 4, whole runs of av1opt gained a
  third of what the clips said, because the CRF moves in quarter steps and the repairs of weak
  stretches eat part of the gain. The same may hold here; nobody has run a whole video through
  libaom or preset 3 with the check and the repairs.
- **libaom was measured against preset 6, not 5.** Against today's default the gain is smaller
  than 21.8% by whatever preset 5 gains over 6 (about 5% on the long clips).

## The cheap half already exists

`--preset 3` works today, on the command line and in the page. If the -16% holds up on whole
runs, most of what libaom offers is there for a quarter of its time and no new code. So the
first step is a measurement, not a feature:

1. Whole runs of two or three real sources at preset 3 against preset 5: bytes, mean, lowest
   5 s window, wall time.
2. If the gain is real, the "mode" may be no more than a named choice in the page ("Smallest,
   about three times slower") that sets the preset, with an honest time estimate in the scan.

## What libaom would take

Only worth starting if step 1 leaves a gap that matters. The encoder is named in three source
files (`src/tools.rs`, `src/search.rs`, `src/main.rs`), but much more leans on SVT-AV1's
behaviour:

- **Parameters.** `-c:v libaom-av1` takes its own options (`-cpu-used`, `-row-mt`, `-tiles`,
  `-lag-in-frames`, `-g`), and its CRF scale is not SVT's: the search's starting point, its
  VMAF-per-CRF slope and the quarter steps (`crf` as a fraction is SVT's) all have to be
  learned again. `--svt-params`, the tuned parameters of `bench` and the trial cache key
  belong to SVT.
- **Repairs.** Weak stretches are encoded again and spliced into the stream at key frames
  (`src/repair.rs`), which relies on closed GOPs of a fixed length, a sequence header at every
  key frame and pieces whose headers agree (`src/sequence_header.rs`). All three have to be
  shown for libaom, or the repair has to be done another way for it.
- **Threads.** libaom uses far fewer threads than the machine has; the measurement above ran
  it with four. Wall time comes down only by encoding several chunks at once, which is what
  `--scenes` does, so the slow mode would probably run per scene with several workers. That
  path joins chunks the same way the repairs do.
- **Estimates and benchmark.** The scan's time estimates, `bench` and the history's
  calibration assume one encoder.
- **Playback.** Any valid AV1 stream plays in Chrome and Firefox whoever encoded it; what has
  to be checked is the same as today (level, 10-bit, the container tags).

## Done when

- A whole-run table for preset 3 (and libaom if pursued) is in the rig's README.
- The owner can pick the slow mode in the page and on the command line, sees beforehand
  roughly how much longer it takes, and the output passes the same whole-video check.
- The end-to-end suite covers one encode in that mode.
