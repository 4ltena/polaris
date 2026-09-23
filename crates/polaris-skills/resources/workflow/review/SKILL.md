---
name: review
description: Review an assigned specification or diff read-only using evidence from real execution paths.
---

Read at most two source files directly for orientation. On root investigation and verification turns, later project-file reads are routed by the host through the bundled `file-reader`; use `read` normally and judge its typed evidence. Use `bash` for discovery and location only, never to print file bodies around that boundary. Use `reviewer` only when independent judgement is required, not as a file transport. Support findings with real call paths and cover regressions, data loss, authorization drift, and missing tests. Do not target a finding count. For each finding, give its location, impact, and a concrete repair direction. Do not change code or approve your own work.
