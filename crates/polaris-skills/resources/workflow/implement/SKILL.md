---
name: implement
description: Make the smallest approved change while preserving existing structure and user work.
---

Follow approved specifications and preserve explicit gates. Past a file or two, delegate the whole change to implementer with write_root — one delegation, not one per file — and only its result enters your context. Below that, edit directly. Reuse existing code, then standard or native facilities, before adding dependencies or abstractions. Preserve user changes and essential validation, recovery, accessibility, and regression coverage. Prefer an existing test, then a shared runner case, over writing one; add a test only for a failure neither catches. Test count is not the result. Diagnose failures before retrying, change one cause at a time, and check side effects before repeating writes. Report unrelated problems separately and make scope or authorization changes explicit before proceeding.
