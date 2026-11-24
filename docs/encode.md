# encode

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
   Where one CRF has to carry its worst stretch (`--scenes`, HDR), and with sections removed,
   clips of 4 seconds, long enough to show a stretch: 8 of them up to about 17 minutes, then one
   for every two minutes of video, 32 at most. `--samples` and `--sample-secs` set the
   number and the length by hand; what they leave out follows the 4-second scheme, and the
   search then goes by the samples as they are, whole-video check or not.
   A video whose first frame is not at zero (Matroska and MPEG-TS files can start late) is
   measured from where it starts; for a Matroska file that takes a look at its first and its
   last packets, a fraction of a second. Matroska states one frame's length, and ffmpeg
   reads that as both the declared and the average rate whatever the frames do; so the
   rate of a Matroska file is read off its first 32 packets, and where those come slower
   than declared, off 32 in its middle too (see [lessons](lessons.md)). Clips, and the chunks of a per-scene encode, are
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
   video can't be scored afterwards (step 5: HDR), one CRF has to carry
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
   say otherwise (see [bench](bench.md)). Two parameters are added to whatever set is in
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
   already. A scored frame more than 25 VMAF under the median of the scored frames within a
   second either side of it marks its group of pictures as well, whatever its windows come
   to: a hard scene pulls the frames around it down too, and a frame that far down by
   itself was wrecked on the way (a decoder of the check handing over broken pictures, or
   the encode holding them; see [lessons](lessons.md)). One such frame takes a point and a
   half off its windows, which most do not show. That group is first encoded again at the
   CRF it had, which mends a fault for no more bytes, then once five steps lower; a frame
   that scores no better there is how the picture is, and the group stays as it was. The
   log and the report say where such frames were found and how many stayed. A low stretch
   of a second or less cannot be told from them and goes the same way. The report lists each
   stretch with its CRF, score and bytes before and after, and the mean and the lowest five
   seconds of the whole video. The mean is checked too. Where it is under the target after
   the repairs, the lowest groups of pictures not yet touched are encoded again, lowest
   first, each only as far as it takes, until the measured mean reaches the target; what was
   repaired stays as it landed. That may take a quarter of the video at most, with one more
   round allowed to lift them half a point higher if the mean is still more than 0.05
   short; the report says how much was encoded again for the mean, at what cost, and whether
   it stayed short. The scores foretell all this by imitating the repairs before any is
   made: if the mean could not be brought up within that quarter, or would end more than 1.5
   above the target while no more than a quarter of the video is to be encoded again below the
   line, the whole video is encoded once more at a CRF moved by the difference, once per
   video. (That much over the target is about a tenth more file than it needs; the second
   encode takes that off for up to twice the time on that video. Less is left as it is.) Windows left less than a point under the line after three
   rounds are kept and noted. Stretches further short get up to three rounds more, by
   themselves; what is still that far short after them is kept as well, at the best encode
   its tries gave it, and the report names each such stretch, as long as they come to no
   more than a twentieth of the video. Past that the video is weak throughout: its weakest
   stretches become samples of a second search, which they hold to its worst sample (two
   points above the line this time) and not to its mean, which goes by the usual samples;
   the video is encoded once more at the lower CRF that search finds, which the report
   says; if that saves less than `--min-savings` the video is skipped, and a second failure
   stands.
   With sections removed the source is decoded through the same picking of frames as the
   encode was (see [Removing sections](#removing-sections---remove)), so that both sides hold
   the same frames in the same order, and the stretches encoded again are cut from that same
   picture; a video too weak for its repairs is searched and encoded again as a whole one
   is, with the same sections removed and each weak stretch a sample from inside one kept part.
   Where frames can't be paired one for one (the two sides of a cut hold different numbers of
   frames, a source in which more than
   one frame in ten stays on screen much longer or shorter than the rest, as in a screen
   recording), in HDR and for videos encoded per scene, it is spot checks at
   fixed positions instead (five seconds at 30% and at 70% of the way;
   one for every ten minutes of a video of 25 minutes or more, eight at most; all of a
   video of ten seconds or less, once). So it is where stretches can't be encoded again (no
   room for them on the drive, say), and the report then says so. A spot check more than 4
   below the target leads to the same second search, with sections removed as well; a video
   encoded per scene fails instead. A video whose picture changes size on the way (a
   recording of a live stream that changed quality, parts joined without encoding them) is
   refused when the check of the whole video stops at the change: an encode has one size,
   the first, and would stretch or shrink the rest. Nothing is written unless all checks pass. The source
   has to be as it was, too. It counts as changed if its size is different, or if its
   modification time is more than two seconds off and a fingerprint of its contents (a hash
   of its size and of a megabyte at its start, middle and end) differs as well; a file only
   touched passes. A changed source fails the encode: no output or report is kept, and
   neither are the chunks of a per-scene encode, which no longer belong to the file as it is
   now. A source that has gone away or cannot be read fails the encode too, but as that may
   only be a drive that dropped out, the chunks are kept and encoding it again goes on from
   them.
6. **Report** – the output plus `<name>.av1opt.json` with every CRF tried, scores, sizes and timings.

## Metadata

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

## Removing sections (`--remove`)

```
av1opt encode talk.mp4 -o /out --remove 0:00-0:42 --remove 12:03.5-14:10 --remove 58:30-end
```

Leaves those parts out of the output (one input file only). Times are `h:mm:ss`, `m:ss` or
seconds, with decimals; `end` (or nothing after the dash) means the end of the video. Sections
may overlap and come in any order. The video is re-encoded anyway, so the cuts are
frame-accurate: the frames of the kept parts are picked from the decoded source and moved back
by what was removed before them (ffmpeg `select`/`setpts`), the sound is trimmed and joined
(`atrim`/`concat`). A frame is removed if it is shown from inside a section; a kept part ends
where its last frame does, picture and sound alike, so that the parts after it stay where the
timeline puts them (a variable-rate source is cut at the times given). A frame held across a
section is shown that much less, as the sound is. Audio is re-encoded to Opus; text subtitles and chapters
are moved to their new times (lines and chapters inside removed parts go); image subtitles
(PGS, DVD) and TTML are dropped. Sample clips and spot checks are taken from kept parts only; a
spot check compares the frames of one kept part with the same frames where the output shows them.
The finished cut is scored along its whole length and its weak stretches encoded again, as a
one-pass encode is (see [encode](#encode), step 5), so its CRF does not have to carry its
worst sample: the source is decoded through the same `select`/`setpts`, its times counted
from the start of the file as the encode counted them (`-copyts -start_at_zero`, and the
sound read beside the picture as the encode read it: ffmpeg starts a transport stream at the
first packet of what it reads), the frames
are counted on both sides, and a group of pictures that holds a join is encoded again from
the frames the output shows there, the end of one kept part and the start of the next. Only
the video stream of the cut is then replaced: its sound, subtitles, chapters and marks stay
as the cut made them. The whole video is encoded once more where the scores say so, with the
same sections removed. A variable-rate source cut at the times given goes the same way, under
the same rule as any other source (no more than one step between frames in ten uneven);
past that, and wherever the two sides of the cut do not hold as many frames, it gets the
spot checks, and one that fails makes its stretch a sample of a second search, as without
sections removed: nothing else holds the CRF of a cut to its worst stretch. A cut is
never skipped for being AV1 already or saving too little, and if the VMAF target is out of reach
it is encoded at the best CRF tried. At least one second of the video has to stay: sections
that remove all of it, or all but a moment, are refused before the encode starts, and the web UI
refuses them when they are queued. A Dolby Vision file is skipped all the same, and sections
can't be removed from a file whose sound or subtitles run more than a second past its picture. Per-scene
mode isn't combined with cuts (one pass instead).
The report lists the removed sections and the parts kept, as cut on the frames; an existing output with other sections removed is only
overwritten with `--overwrite`. In the web UI, **Choose sections to remove…** in the encode dialog (one file) has
a player with Mark start / Mark end (keys I and O), frame stepping, and playback that skips the
marked parts; Compare maps the cut output back onto the source.

## Per-scene CRF (`--scenes`)

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

## Exit status

`encode` ends with 0 when no file failed (files skipped, already done or needing confirmation are no failure), 1 on
an error that stopped the run, 2 on wrong usage, 3 when some files failed or an output already
there could not be verified and the others were dealt with, and 130 or 143 when Ctrl-C or
SIGTERM stopped it. `dups` ends with 3 when more than half of the videos could not be read: it
says so after the results, which then mean little.

A path named on the command line that does not exist or cannot be read stops `scan`, `dups`,
`bench` and `encode` with status 1 before anything is done. A folder further down that cannot be
read gets a warning, the run goes on without it, and the summary counts it.

## File names

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

## `encode` options

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
