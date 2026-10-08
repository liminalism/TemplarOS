# TemplarOS integration planning pack

Prepared October 5, 2026 from the two supplied agent packs and the earlier display alpha.

Read **PLAN.md** for decisions and the intended desktop behavior. Use **BACKLOG.md** for twenty bounded implementation slices, dependencies, existing AKR parents and acceptance gates. **EVIDENCE.md** contains twenty-five source findings with original file/line excerpts. **source-manifest.json** records hashes of the cited files.

This is a source review and proposed implementation plan. No source changes, AKR imports, compilation, browser tests, hardware display changes, image boot tests, repository writes or live-session deployment were performed. Existing comments and generated planning can lag code; the plan identifies several such conflicts rather than treating every comment as current implementation.

The proposed order starts with browser file dialogs and PCManFM interoperability. Compositor-owned HDMI recovery, native settings/toolkit work and system ownership enforcement are separate lanes with explicit dependencies. Native chooser work and read-only-root migration do not block the first working browser upload.
