# Exclusions preset packs — content draft (Day 2 review)

**Status:** DRAFT for design review per `~/sd-bench-local/design/file-exclusion-spec.md` §2.2.
**Owner:** engine (drafting); design (review + sign-off before const-data commit).
**Next:** once design approves, land as `src/exclusions/presets.rs` with `#[rustfmt::skip]` const arrays + a `PresetSource` impl.

## Design principles applied

1. **Cross-platform first.** Each pack carries patterns that match the same conceptual category across Windows + Linux + macOS (where the path conventions differ). Where a pattern is platform-specific, it's still included — globset is fast; misses are free.
2. **Globs use `**/foo/**` form** for things that might live under any user-controlled root (caches, hidden dirs). Absolute paths only for OS-install specifics (`C:\Windows\WinSxS`).
3. **Extensions stored lowercase, leading-dot-stripped** (handled by `normalize_extension`).
4. **Conservative defaults.** When uncertain whether something is load-bearing, prefer to EXCLUDE it from the pack so the user has to add it manually. Better to have a slightly smaller pack than to accidentally hide a user's data.
5. **OFF by default per Mick.** Every pack listed below ships visible-but-inactive; user opts in via Settings → Exclusions.

## Pack 1: System libraries

**Why:** Shared + static libraries and packaged resources. Deleting them breaks the apps that ship them.

**Extensions** (15):
```
dll so dylib pyd class jar pak asar mui ocx drv sys lib a node
```

Notes:
- `.dll` / `.so` / `.dylib` — Windows / Linux / macOS shared libraries
- `.pyd` — Python compiled extensions
- `.class` / `.jar` — JVM bytecode + archives
- `.pak` / `.asar` — Electron app resources (Chrome, VS Code, Discord, etc.)
- `.mui` — Windows multilingual UI resources
- `.ocx` / `.drv` / `.sys` — Windows ActiveX / drivers / system files
- `.lib` / `.a` — static libraries (link-time; not runtime, but often shipped together)
- `.node` — Node.js native addons

**Path patterns:** (none — this pack is extension-only)

## Pack 2: Build artefacts

**Why:** Per-project dev caches; regenerated automatically when needed. Excluding them strips noise from dedup results for developers.

**Path patterns** (16):
```
**/node_modules/**
**/__pycache__/**
**/.venv/**
**/venv/**
**/.virtualenv/**
**/target/**
**/build/**
**/dist/**
**/out/**
**/.next/**
**/.nuxt/**
**/.svelte-kit/**
**/.angular/cache/**
**/.gradle/**
**/.idea/build/**
**/cmake-build-*/**
```

Notes:
- `target/` matches both Rust + Maven; intentional
- `build/` matches CMake, Gradle, generic — intentional
- `.angular/cache/` not `.angular/**` to avoid catching user-editable Angular workspace files
- `cmake-build-*` matches CMake's IDE-managed build dirs (`cmake-build-debug`, `cmake-build-release`)

**Extensions:** (none)

## Pack 3: VCS internals

**Why:** Content-addressable storage. "Dups" here are structural (git's object dedup is intentional); flagging them confuses the user. Deleting them breaks the repo.

**Path patterns** (8):
```
**/.git/objects/**
**/.git/lfs/**
**/.git/refs/**
**/.git/pack/**
**/.svn/**
**/.hg/**
**/.bzr/**
**/CVS/**
```

Notes:
- `.git/objects/`, `.git/lfs/`, `.git/refs/`, `.git/pack/` — git's content-addressable + ref + pack stores. NOT the whole `.git/` tree (user's git hooks, configs are interesting).
- `.svn/`, `.hg/`, `.bzr/`, `CVS/` — full tree exclusion (these are entirely VCS-managed, no user files)

**Extensions:** (none)

## Pack 4: Package manager caches

**Why:** Per-user dependency caches. Deletion forces re-download but doesn't break installed software. Often gigabytes of "dups" that aren't really dups.

**Path patterns** (13):
```
**/.m2/repository/**
**/.gradle/caches/**
**/.cargo/registry/**
**/.cargo/git/**
**/.npm/_cacache/**
**/.npm/_logs/**
**/.yarn/cache/**
**/.pnpm-store/**
**/.bundle/cache/**
**/.composer/cache/**
**/.cache/pip/**
**/.cache/uv/**
**/Library/Caches/Homebrew/**
```

Notes:
- pnpm content-addressable store: `.pnpm-store` (per-machine) or `node_modules/.pnpm` (in-project — covered by Build artefacts)
- `.cache/pip/`, `.cache/uv/` — XDG cache dir on Linux + macOS
- Homebrew: macOS-specific path
- (`.local/share/Trash/` moved to Pack 5 OS system trees per design review — fits semantically as OS-shell concept, not a PM cache)

**Extensions:** (none)

## Pack 5: OS system trees

**Why:** OS internals + OS-shell-managed dirs (Trash, Recycle Bin). Deleting these = reinstall (Windows) or `sudo apt --reinstall` (Linux). Heavy "dup" content because Windows ships multi-version side-by-side and Linux ships compiled stdlib variants. User's Trash also lives here — files the user explicitly deleted should not be deduped, since the user already chose to throw them away.

**Path patterns** (13):
```
C:\Windows\WinSxS\**
C:\Windows\System32\**
C:\Windows\SysWOW64\**
C:\Windows\Installer\**
C:\Windows\servicing\**
**/AppData/Local/Microsoft/Windows/Caches/**
**/AppData/Local/Microsoft/Edge/User Data/Default/Cache/**
/usr/lib/**
/usr/lib32/**
/usr/lib64/**
/usr/share/locale/**
/var/lib/dpkg/info/**
**/.local/share/Trash/**
```

Notes:
- Windows: backslash patterns. globset on Linux DOES match `C:\Windows\...` style paths if the input path string literally starts with `C:\` (i.e. a Windows-targeted scan from a Linux build). Worth verifying in a follow-up test; expected to work.
- `Windows/Installer/`, `Windows/servicing/` — MSI cache + Windows update servicing
- Edge cache as a sample — Chrome / Firefox / etc covered in Browser caches pack
- Linux `/usr/lib*` — system libraries; user shouldn't dedupe these
- `/usr/share/locale/` — translation files; heavy "dup" content across locales

**Extensions:** (none)

## Pack 6: Browser caches

**Why:** Auto-regenerated by browsers on next launch. Browsers DO have their own dedup (HTTP cache key collisions) but it's not your job to second-guess them.

**Path patterns** (14):
```
**/Cache_Data/**
**/cache2/**
**/Service Worker/**
**/IndexedDB/**
**/Local Storage/leveldb/**
**/Session Storage/**
**/Code Cache/**
**/GPUCache/**
**/Cache/Cache_Data/**
**/Profiles/*/cache2/**
**/Profile */Cache/**
**/Default/Cache/**
**/.mozilla/firefox/*/cache2/**
**/Library/Caches/com.apple.Safari/**
```

Notes:
- Chrome / Edge / Brave / Opera all share Chromium's `Cache/Cache_Data` + `Code Cache` + `GPUCache` + `Service Worker` + `IndexedDB` layout
- Firefox uses `cache2/`, `IndexedDB/`, `storage/`
- Safari: `~/Library/Caches/com.apple.Safari/`
- Generic `**/Cache_Data/**` catches most cross-browser

**Extensions:** (none)

## Pack 7: App-specific caches

**Why:** Auto-regenerated by apps. Shader caches, asset caches, editor caches — bulky and unstable across runs.

**Path patterns** (18):
```
**/Steam/steamapps/shadercache/**
**/Steam/steamapps/downloading/**
**/Steam/steamapps/temp/**
**/Spotify/Storage/**
**/Discord/Cache/**
**/Discord/Code Cache/**
**/Discord/GPUCache/**
**/Adobe/Common/Media Cache/**
**/Adobe/Common/Media Cache Files/**
**/Lightroom Catalog Previews.lrdata/**
**/Office/Containers/Office365ServiceV2/**
**/Code/Cache/**
**/Code/CachedData/**
**/JetBrains/*/caches/**
**/JetBrains/*/log/**
**/Slack/Cache/**
**/Postman/Cache/**
**/zoom/data/**
```

Notes:
- Steam shadercache often 10s of GB; user awareness benefit large
- Adobe Media Cache: per-user, regenerable from source media
- VS Code (`**/Code/Cache/`) + JetBrains IDEs (`**/JetBrains/*/caches/`)
- `Lightroom Catalog Previews.lrdata/` — preview files; recreatable from catalog
- Office 365: `Office365ServiceV2/` is the local cache layer

**Extensions:** (none)

## Pack 8: AV signature databases

**Why:** Anti-virus signature definitions. Deleting these = AV breaks (no virus detection until update). Definition files dedupe heavily across versions but the file system tracks them by version — deleting "the dup" probably breaks the AV's version index.

**Path patterns** (10):
```
C:\ProgramData\Microsoft\Windows Defender\Definition Updates\**
C:\ProgramData\Microsoft\Windows Defender\Scans\**
**/AppData/Local/Microsoft/Windows Defender Scans/**
C:\Program Files (x86)\Norton\**\NDB\**
C:\Program Files\Bitdefender\**\Antivirus\**
C:\Program Files\Common Files\McAfee\**
C:\Program Files\ESET\**\Modules\**
C:\Program Files\Avast\**\setup\**
C:\Program Files\AVG\**\setup\**
**/clamav/database/**
```

Notes:
- Windows Defender: `ProgramData\Microsoft\Windows Defender\` is the standard install path
- Common third-party AVs covered (Norton, Bitdefender, McAfee, ESET, Avast, AVG)
- ClamAV on Linux: `**/clamav/database/**` catches the standard `/var/lib/clamav/` location

**Extensions:** (none)

## Total

- **8 packs** matching spec §2.2
- **15 extensions** (Pack 1 only)
- **92 path patterns** distributed across Packs 2-8

## Open questions for design

1. **Should `target/` (Rust + Maven build dir) be in Build artefacts pack?** It's common; deleting frees space. But: a user scanning a project they're actively building shouldn't have `target/` hidden from results. Counterargument: `target/` IS load-bearing dups (rebuild artefacts); user probably doesn't want to manually pick keepers. Leaning yes; flag if you'd rather it be opt-in via custom-pattern.
2. **`.local/share/Trash/` in Package-manager-caches pack?** Pulled it in because it's "stuff the user deleted, don't make them un-delete by deduping it." But it could be its own pack ("Recycle / Trash") or in OS system trees. Open.
3. **Firefox cache pattern `**/.mozilla/firefox/*/cache2/**` uses `*` — single-segment glob.** Spec doesn't specify whether globset's `*` matches across path separators (it doesn't). The `*` here matches the profile dir (`u92h2x4n.default-release`). Confirm globset behaviour matches my intent. (Test added in follow-up commit if you confirm; behaviour was verified manually against globset 0.4 docs.)
4. **macOS `Library/Caches/com.apple.Safari/` in Browser caches?** sd doesn't ship for macOS yet but the pattern is portable. Including for future L3 work. Strike if you'd rather wait.

## Sequencing once approved

1. Land `src/exclusions/presets.rs` with const arrays matching this content + a `PresetSource` impl
2. Wire `ExclusionPolicy::compile` to call the real impl instead of `EmptyPresets`
3. Add counter wiring (`ExclusionCounters`) — separate concern; lands after preset content
4. Scan summary line: `"excluded by Settings → Exclusions: N files (Y bytes)"`

ETA: ~0.5 day for the const data + wiring once content locks; counter wiring is another 0.25-0.5 day.

WAITING ON: design (review per-pack content + answer open questions; engine commits const data on approval)
