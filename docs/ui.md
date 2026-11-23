# ui (optional)

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
  plays the copy in the trash. A replacement or undo that was stopped half-way (killed, or
  the power went) is shown as such, with where the files are and what putting them right
  does, and a **Put right** button that does it (the rules of [replace](replace.md)); nothing
  is moved before that is pressed, and files no longer where that run would have left them
  get no button.
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
  [replace](replace.md)), all ticked but those whose output leaves sections of the original out
  (marked, and said above the list): those are replaced only when ticked one by one, and
  **All / none** leaves them alone. It works through the ticked ones one by one and keeps the ones
  that failed on screen with the reason. Replaced encodes carry a "replaced" mark, and pairs
  stopped half-way a "stopped half-way" one. An original
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

## Desktop app (optional)

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

## Safety

- `encode` never modifies, moves or deletes an input. Only `replace` (and its buttons in the UI)
  moves originals, and only to the trash.
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

## Saved files and versions

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

## Debugging

`AV1OPT_DEBUG=1` prints every ffmpeg command; files in the temporary and work folders appear
in them as `/proc/<number>/fd/<number>/...` (see Safety). `--keep-temp` keeps the temporary folder in the
output folder (`.av1opt-tmp-<number>`, until a later run there clears it), with the partial
encode if the encode fails, and the work folder of a per-scene encode. Sample clips are deleted
all the same once the search is done with them.
`cargo test --features ui` also runs `tests/ui_api.rs`, which starts the real `av1opt ui` and
checks sign-in, request checks, media access, the job queue, its pause and hours, the duplicate search (and what its
trash action refuses), replacing an original and putting it back (and which reports may do that)
and the benchmark's requests and tuned settings over HTTP (no ffmpeg needed).
