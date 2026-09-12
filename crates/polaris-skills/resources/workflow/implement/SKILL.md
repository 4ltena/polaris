---
name: implement
description: Make the smallest approved change while preserving existing structure and user work.
---

Handle local, low-risk, reversible edits directly; follow approved specifications for broader changes and preserve explicit gates. Keep routine implementation in the main agent. Delegate only independent, bounded work when its benefit exceeds coordination cost; keep coupled changes sequential. Reuse existing code, then standard or native facilities, before adding dependencies or abstractions. Preserve user changes and essential validation, recovery, accessibility, and regression coverage. Diagnose failures before retrying, change one cause at a time, and check side effects before repeating writes. Report unrelated problems separately and make scope or authorization changes explicit before proceeding.
