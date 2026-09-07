---
"@googleworkspace/cli": patch
---

Collapse a nested `if` inside the `json` match arm of the Apps Script file walker into a match guard, fixing a `clippy::collapsible_match` failure under `-D warnings`.
