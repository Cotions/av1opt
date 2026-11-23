# Lessons baked in

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
  below), the CRF comes down this way only in HDR.
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
- **Where a transport stream starts depends on what is read from it.** ffmpeg counts the time
  of an MPEG transport or program stream from the first packet of the streams it reads, not
  from where the file says it starts. The encode of a video with sections removed reads sound
  and picture; the whole-video check and the stretches encoded again read the picture alone,
  and picked the kept frames by a clock that ran behind the encode's by as much as the sound
  starts before the picture. With 23 ms of that (what ffmpeg itself writes) at 24 frames a
  second, they took the first frame of each removed section for a kept one and left out the
  first frame after it: as many frames on both sides, so the count told nothing, and a group
  of pictures encoded again at a join put a removed frame into the file. Both read the sound
  as well now (one packet of it for a stretch, copied to nowhere), which gives them the
  encode's clock by the same rule that gave the encode its own.
- **ffmpeg ignores `-svtav1-params` names it can't parse.** A typo only prints a warning and the
  encode runs without the option, so parameters are tried on a few generated frames first.
