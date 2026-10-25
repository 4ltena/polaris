---
name: envkit
description: Provision a supported development environment through the fixed host runner. Use for environment setup, not ordinary project implementation.
---
Preserve the requested profile, target, explicit version and existing lockfiles.
Call the typed environment tool once. Its host owns compatibility, plan approval,
pinned downloads, offline installation and verification; do not generate setup scripts.
If the tool or profile is unavailable, report that condition without claiming installation.
Report the saved receipt, selected versions and actual verification result.
