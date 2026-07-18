# Backlog

## Portfolio audit backlog — 2026-07-17
_Findings from a 2026-07-17 code audit, preserved for later._

### Later / deferred
- **[low/S]** docs/tauri-migration-plan.md references app/ path that was relocated to root
  - Fix: Migration is complete (app/ relocated to root per plan step 6; app/ dir absent). Add a 'COMPLETED' banner at top of docs/tauri-migration-plan.md, or leave as historical artifact. Lines 46,62,66,88,152 reference the intentional temp app/ path.
