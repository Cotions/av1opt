# adopt

```
av1opt adopt ~/av1opt-out            # lists what it would add to the record
av1opt adopt ~/av1opt-out --apply
```

Takes outputs that are not on av1opt's private record (see [replace](replace.md)), such as those
made before it kept one, onto it, so that the page can replace them and put their originals
back. It reads the reports under the folders named, lists what it would add, and writes
nothing without `--apply`, and then only to the record; nothing is adopted without this
command. Naming a folder here says that you trust the reports in it, as naming it to `replace`
does. As with `replace`, a report is followed only to originals inside the folders you name
(or anywhere with `--anywhere`). A report has to hold the original's date and a fingerprint or
hash of it, which reports written before 2026-11-18 lack (`replace --accept-changed-date` still
replaces those), and a report whose output or original fails the checks `replace` makes is
refused. So is one that `replace` would not follow on its word: the report, and its output
while that is still in the output folder, have to be your own files in a folder nobody else can write to (or one with the sticky bit),
unless `--trust-reports` is given, and two reports for one original are both refused. An
output that already replaced its original is taken too if it is where `replace`
puts it, in the original's place, and the file in the trash says it came from there and is,
read in full, the one the report describes.

| Option | Default | What it does |
|---|---|---|
| `--apply` | off | Write to the record; without it the command only lists. |
| `--anywhere` | off | Follow reports to originals wherever they are, not only in the folders named. |
| `--trust-reports` | off | Take reports where others could have put them there or changed them, as for `replace`. |
