# replace

```
av1opt replace ~/av1opt-out                # lists what would happen, changes nothing
av1opt replace ~/av1opt-out --apply        # does it
av1opt replace ~/av1opt-out --undo [--apply]
```

`encode` leaves the library as it was: outputs go to their own folder, so no space is saved
yet. `replace` is the second half, for when you have looked at the
results: each original is moved to the trash and its AV1 file gets its name (with the
output's extension) in its folder. Name output folders, single reports (`.av1opt.json`) or
single output files.

- Without `--apply` it only lists: what can be replaced with sizes, what stays and why, and what
  was replaced earlier.
- An output is only used while its report still matches both files: the original and the
  output must be there with the sizes the encode recorded, and the output must be the smaller
  one. An original with another modification time is judged as when `encode` resumes: one only
  touched or copied is replaced, while one rewritten, one that cannot be read and one whose
  report has no fingerprint to tell by are refused, unless you say that only the date changed
  (`--accept-changed-date`, or **Replace original…** on the Compare page, which first says what
  cannot be checked). A report that holds nothing of the original but its size and date (one
  written before 2026-11-18) cannot tell it from another file of that size and date, so its
  original stays where it is whatever its date, unless you say the same. Originals that are
  links, reports without full paths (very old ones) and a new name that is already taken in the
  original's folder are refused too.
- An output with sections removed takes its original's place like any other, and the list
  says so. The removed sections are then only in the original in the trash.
- A report is a file like any other, so it is only followed where `encode` writes one: next
  to its output, under the output's name. Both files it names have to be videos. A report
  that turns up in some other folder and points at files elsewhere moves nothing.
- av1opt keeps a private record of what it made and moved, `ledger.jsonl` in its data folder
  (`~/.local/share/av1opt`, for your user only): a line for each finished encode (output, report,
  original, what tells the original from another file, a SHA-256 of the output) and for each
  replacement and undo. A report on record is followed wherever its original is, because the
  record vouches for that path. Where the record holds an output, or any output made from its
  original, the record decides, whatever folders are named: all of the output is read and has
  to be the file av1opt made, and a report that names another output for an original on record
  is not followed (if both are encodes you made, keep the one on record and remove the other
  output and its report). A report cannot
  get past the record by naming a file another way (through a link to its folder, or
  `/proc/self/root`): paths are compared with their folders resolved (the record's own too,
  as it is read: a folder moved since with a link left at its old name, or a line an earlier
  version wrote as a report spelled it, is still one file), and every output and
  original on record is also compared by the file itself, so a second mount of a folder, a
  second link, or a drive that takes two spellings of a name for one (letter case, accents
  written two ways) does not make one file two. (A folder reached once on its drive and once
  over the network is two drives to the system: a pair named the second way is judged as one
  the record knows nothing of, by the rule below.) `--trust-reports` never counts against the
  record.
- A report the record knows nothing of is followed only to originals inside the folders you
  name (name the originals' folder as well, or add `--anywhere`), and only if it and its output
  are your own files in a folder nobody else can write to, or one with the sticky bit, which
  keeps everyone to their own files. Anyone who can put files in the output folder can copy a
  genuine report there, fingerprint of the original and all, and put a file of their own of the
  stated size beside it. A folder shared with a group (mode `2775`, say) counts as one others
  can write to; the list names the group, and either `chmod +t` on the folder (where everyone
  who uses it only renames or removes their own files) or `--trust-reports` (if you trust
  everyone in the group) lets the pairs through. A group counts as yours alone only if it is
  your own, lists nobody else, and no other user in `/etc/passwd` has it as their own (users
  kept in LDAP and the like are not looked through). A file or folder on an NFSv4 drive with an
  access list counts as one others may write to whatever its mode, since av1opt does not read
  such lists: there too `--trust-reports` is the way for pairs not on record. `--trust-reports` takes such reports at their word all the same. On
  a drive that keeps no owners or permissions of its own it is the only way: FAT, exFAT and
  NTFS drives, SMB (Windows) shares and FUSE drives (ntfs-3g, rclone, sshfs and the like) show
  every file as the mount says, often as your own in a private folder, whoever put it there,
  so av1opt does not take a report there for yours. On other network drives (NFS) what is
  shown is what the server says, and users of other machines may be able to write there
  whatever it shows.
- Two reports the record knows nothing of that name the same original are both refused, each
  naming the other: one of them is not what it says. Remove the one you do not want.
- Outputs that are not on record, such as those made before it began, are taken onto it by
  [`av1opt adopt`](adopt.md).
- Nothing is deleted and no file is written over. The original goes to the trash of the drive
  it is on (as in the duplicates view), and the space is freed when you empty the trash. If the
  output can't be put in place after that, the original is taken back out of the trash. An
  original whose name is near the file system's limit gets a shortened name in the trash and
  still comes back under its own with `--undo`.
- An output on another drive than the original is copied next to it under a hidden name
  first (after a look at the free space), so a copy that fails leaves the original alone; the
  file in the output folder is removed once the copy has its final name. On the same drive it
  is a rename and takes no time.
- The new file gets the original's permissions and keeps the output's date. Subtitles, covers
  and other files next to the original are not touched: they still match by name, unless a
  player wants the extension to be the same.
- The report stays in the output folder and records where both files went (`replaced`: the
  time, the output's old path, the place in the trash). `--undo` uses that to put things back
  while the original is still in the trash: the AV1 file returns to the output folder, the
  original to its place. After the trash is emptied there is nothing to undo. The trash has
  to agree: a file goes back only if the trash's own record says it came from that place, and
  only if all of it is still what was recorded of the original (read whole against its hash, or
  against its samples where a report has only those), whatever its date says. `--undo` asks
  of a report what replacing does: where av1opt's record holds the replacement, the report has
  to say what the record does (where the original went in the trash, where the AV1 file came
  from); where it holds nothing, the original's place has to be in the folders named and the
  report your own file in a folder nobody else can write to, or `--trust-reports` given.
  Otherwise a copy of a report in a shared folder could send your AV1 file out of the library
  to a name of someone else's choosing.
- av1opt's own record notes each replacement and undo before the report is rewritten. A
  report left behind (the disk was full, or the run was killed in between) is set right by
  the next `replace`, `--undo` or the page, once the record, the trash and the files (read
  whole) all show what happened; a replacement whose report could not be written ends with
  an error that says where both files are and what puts them back.
- Before its first move, a replacement and an undo each write a line to av1opt's record,
  flushed to the drive, with what tells their moves from anyone else's (the two places of the
  AV1 file and its hash, the original as it was checked, then its name in the trash before it
  goes there). One stopped between two of its moves (killed, or the power went) is put right
  by the next `replace --apply`, `--undo --apply` or the page's button, by that line and the
  files read whole, never by the report's word: a replacement is put back as it was before it
  (the original back in its place, the AV1 file back in the output folder, a copy of it on
  another drive removed), an undo with the AV1 file still in the original's place goes back
  to the replacement, and one with the AV1 file back in the output folder is finished. A
  replacement that went through, with its report saying so, whose own line on record of that
  could not be written is written down once the trash entry and the AV1 file are read whole
  and are the recorded files; then it can be put back. An original with two outputs (two
  output folders) is replaced by one run at a time: a run that finds, once its own line is
  written, a run of another of its outputs under way or stopped half-way leaves the original
  to that one and says so, and each run's end on record closes its own line only. `--apply`
  then does what was asked; without it the list says what was stopped half-way and where the
  original is. A pair with no such line, or whose files are no longer as that run would have
  left them (moved by hand since, or not the recorded files), is left as it is and said to
  be: an original thrown away by hand with its AV1 file kept is "gone", as it is. A
  replacement or undo that ends with an error and nothing moved (the trash refuses, the
  library is full) writes down that it is over. A replacement stopped before the original's
  name in the trash was written down had not moved it: with a file still in the original's
  place it is over, whatever was written into that file since, and with the original gone
  from its place it is said and left as it is (the line with the name may not have reached
  the drive, and the original may be in the trash). One stopped half-way whose original was
  since taken out of the trash by hand to its place, with the AV1 file still in the
  original's folder (a replacement that had placed it, an undo that had not taken it back
  yet), has the AV1 file taken back to the output folder (a copy of it left in the
  original's folder by a move across drives is removed instead), and an undo is then written
  down as done. One
  stopped half-way whose original was since emptied from the trash, with the AV1 file at one
  of its two places, has nothing left to put back: that is said, and putting it right has
  the record and the report say where the only video left of the two is. An AV1 file a
  replacement had placed stays in the original's folder, and the pair is written down as
  replaced (one copied there from another drive, with the AV1 file still in the output folder
  too, keeps the one in the library, and the other, a copy of it, is removed); one an undo had taken back to the output folder goes to the original's folder
  again, where the record has it; otherwise only the end of the run is written down (with
  the trash folder itself not there, a drive not mounted just now, it is left as it is). On
  the page, replacing and putting back wait, as "Put right" does, while the encode queue
  holds a file of a pair stopped half-way. Nothing is
  put right in a folder on ZFS without `--allow-zfs` (an undo finished so rewrites its report in
  the output folder, which `--undo` does not write to there either; a replacement's report is
  rewritten wherever it is, as `replace --apply` does), a copy is removed only if the file it is
  a copy of is still there and is another file (not the same file reached by a second path),
  and a pair that could not be put right makes the run end in failure. Records written by an
  earlier version have no such lines and never set this off; an earlier version reading a
  newer record passes over them, as it does any line it does not know.
- Originals on ZFS are left alone unless `--allow-zfs` is given, and are put back there by
  `--undo` only with it.
- A later `encode` or `scan` over the library sees the replaced files as AV1 and skips them.

## `replace` options

`av1opt replace <paths>...`: output folders, the folders of the originals, reports or output files.

| Option | Default | What it does |
|---|---|---|
| `--apply` | off | Do it; without it the command only lists. |
| `--undo` | off | Take replaced originals back out of the trash and return the AV1 files (acts only with `--apply`). |
| `--allow-zfs` | off | Also replace originals stored on ZFS. |
| `--accept-changed-date` | off | Take an original whose date changed as the file that was encoded, where its report cannot tell; originals seen to differ are still refused. |
| `--anywhere` | off | Follow reports to originals wherever they are, not only in the folders named. |
| `--trust-reports` | off | Take reports that are not on record at their word where others could have put them there or changed them: in a folder others can write to, files of another user's, or a drive that keeps no owners of its own (FAT, exFAT, NTFS, SMB, FUSE). |
