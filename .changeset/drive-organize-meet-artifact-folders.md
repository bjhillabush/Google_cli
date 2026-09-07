---
"@googleworkspace/cli": minor
---

`drive +organize` now also empties Google Meet's per-meeting folders. Meet
stopped dropping recordings loose in "Meet Recordings" and started creating a
top-level "Google Meet" folder with one subfolder per call, which the command
previously ignored. It now sweeps those subfolders into the same client
folders, leaves anything it cannot identify loose in "Meet Recordings" as
before, and trashes the per-meeting folders it emptied (opt out with
`--keep-empty-folders`). Files are still never deleted.

Client matching is also stricter: names are compared position by position, so a
title carrying a surname that disagrees with the folder's no longer falls back
to matching on the first name alone. That had filed "Brian Lee" into Brian
Bardi's folder and "Justin Liu" into Justin Christian's. Shortened and extended
spellings still match ("Ayo" to "Ayodeji Ejidiran", "Whitney Gilbert-Clarkson"
to "Whitney Gilbert").
