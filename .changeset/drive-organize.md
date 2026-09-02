---
"@googleworkspace/cli": minor
---

Add `drive +organize` to match Meet recordings into client subfolders. Client-name matching is word-boundary aware and ignores punctuation, so stylized meeting titles (`mus-tafa-`) still reach their plainly-named folder (`Mustafa`), while short or partial names never match a longer unrelated surname. The account owner is skipped when they are listed first in a recording title. A network failure on one move is recorded and the batch continues, so an unattended run still processes every remaining file. Files are only ever moved, never deleted; unmatched files are reported and left in place.
