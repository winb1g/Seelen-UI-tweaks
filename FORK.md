# winb1g/Seelen-UI-tweaks — Fork Notes

Personal fork of [eythaann/Seelen-UI](https://github.com/eythaann/Seelen-UI) with bug fixes and small enhancements. The
overriding goal is that **upstream can be merged at any time with no (or trivial) conflicts**.

Docs: [first-time setup](fork/docs/first-time-setup.md) · [releasing](fork/docs/releasing.md) ·
[installing & updating](fork/docs/updating.md) · [CI](fork/docs/ci.md)

## Rules

1. **All fork code lives in [`fork/`](fork/)**, mirroring the upstream area it extends (`fork/weg/` for the dock,
   `fork/toolbar/` for the toolbar, etc.). Upstream has no such folder, so it can never conflict.
2. **Upstream files only get hooks**: the smallest possible change (ideally an import plus one line) that calls into
   `fork/`. Every hook is listed in the table below.
3. **Never edit upstream-owned shared files** such as translation files (`i18n/translations/*.yml`), generated code
   (`libs/core/src/handlers/commands.ts`, `gen/`) or lockfiles, unless a fix truly requires it. Fork UI strings live
   inside the fork module.
4. **Prefer extension points** that need no hook at all (themes, plugins, widgets) when they can do the job.
5. **Bug fixes that upstream would want** may be sent upstream as PRs. Once merged there, drop them from the fork. When
   such a fix has to edit upstream code directly, list every touched file under [Upstream patches](#upstream-patches).
6. **Fork docs and CI are fork-owned files too**: `FORK.md`, `fork/docs/`, `.github/README.md`, `CLAUDE.md` and
   `.github/workflows/fork-*.yml`. Upstream docs (`README.md`, `documentation/`, `AGENTS.md`) and workflows are never
   edited (`CLAUDE.md` is gitignored upstream, so it is tracked with `git add -f`). CI enforces rules 2, 3 and 5 (see
   [`fork/docs/ci.md`](fork/docs/ci.md)).

## Workflow

- Branch per change from `master` (`feat/...`, `fix/...`) and open a PR into `winb1g/Seelen-UI-tweaks:master`. Upstream
  CI and fork CI both run on it ([`fork/docs/ci.md`](fork/docs/ci.md)).
- Every merge into `master` publishes a release that installed copies update to
  ([`fork/docs/releasing.md`](fork/docs/releasing.md), [`fork/docs/updating.md`](fork/docs/updating.md)).
- Upstream is merged monthly by `fork-sync.yml`, which opens a `sync/upstream-*` PR, or by hand:

  ```bash
  git fetch upstream
  git merge upstream/master
  node fork/scripts/reapply-hooks.ts  # only if the merge conflicts
  ```

  A hook file conflicts when upstream edits the lines next to a hook. `reapply-hooks.ts` resolves it by taking
  upstream's version and putting the hooks back; `fork-sync.yml` runs it on its own and only fails when something else
  conflicts.

- After every upstream merge:
  - CI checks that every hook below still exists (`node fork/scripts/fork-check.ts hooks`).
  - Check for **new upstream workflows** in `.github/workflows/` and disable them in the Actions tab (see
    [Upstream workflows](#upstream-workflows)).

## Hooks into upstream files

| Upstream file                                                   | Hook                                                     | Feature                         |
| --------------------------------------------------------------- | -------------------------------------------------------- | ------------------------------- |
| `src/background/main.rs`                                        | `#[path = "../../fork/background/mod.rs"]` + `mod fork;` | [Backend entry](#backend-entry) |
| `src/background/main.rs`                                        | `fork::init();` at the end of `setup`                    | [Backend entry](#backend-entry) |
| `src/ui/svelte/weg/components/items/UserApplicationItem.svelte` | `{@attach appVolumeControls(` + import, on `.weg-item`   | [App volume](#app-volume)       |
| `src/background/utils/updater.rs`                               | `crate::fork::release::SIGN_PUB_KEY`                     | [Releases](#releases)           |
| `src/background/utils/updater.rs`                               | `crate::fork::release::NIGHTLY_ENDPOINT`                 | [Releases](#releases)           |
| `src/background/utils/updater.rs`                               | `crate::fork::release::RELEASE_ENDPOINT`                 | [Releases](#releases)           |
| `src/background/utils/integrity/checksums.rs`                   | `crate::fork::release::SIGN_PUB_KEY`                     | [Releases](#releases)           |

The first `code span` of the _Hook_ column must appear verbatim in the file: CI fails when an upstream merge drops it.
Every file in this table needs a recipe in [`fork/scripts/reapply-hooks.ts`](fork/scripts/reapply-hooks.ts) that puts
its hooks back after a conflict (the fork tests fail otherwise). Check it with
`node fork/scripts/reapply-hooks.ts check upstream/master`.

## Upstream patches

Upstream files edited directly (a fix that can't live behind a hook), until upstream merges an equivalent. CI fails when
a PR changes an upstream file listed in neither table.

| Upstream file                           | Change                                                                         | Upstream PR |
| --------------------------------------- | ------------------------------------------------------------------------------ | ----------- |
| `libs/core/src/state/icon_pack.ts`      | Exe filename fallback skips absolute entries of other installs (same exe name) | —           |
| `libs/core/src/state/icon_pack.test.ts` | Test for the filename fallback fix                                             | —           |

## Upstream workflows

Upstream's workflows stay in the repo untouched (so merges never conflict) but are **disabled in the Actions tab**,
except `ci.yml`, which runs on every PR as the "don't break upstream" check:

| Workflow                   | State    | Why                                                         |
| -------------------------- | -------- | ----------------------------------------------------------- |
| `ci.yml`                   | enabled  | Upstream's own checks, run on every PR and before releases. |
| `nightly.yml`              | disabled | Needs upstream secrets; replaced by `fork-release.yml`.     |
| `release.yml`              | disabled | SignPath / Microsoft Store / npm, upstream only.            |
| `release_no_signed.yml`    | disabled | Replaced by `fork-release.yml`.                             |
| `msix.yml`                 | disabled | Microsoft Store, upstream only.                             |
| `publish-core.yml`         | disabled | Publishes `@seelen-ui/lib` to npm, upstream only.           |
| `discord-notify.yml`       | disabled | Upstream's Discord.                                         |
| `dependabot-automerge.yml` | disabled | Dependency updates come from upstream merges.               |

## Features

### Releases

[`fork/background/release.rs`](fork/background/release.rs) — the fork's signing public key and update endpoints.
Upstream's updater and bundle integrity check read them through the hooks above, so fork builds verify against the fork
key and update from this repo's releases instead of upstream's. Built and published by
[`fork-release.yml`](.github/workflows/fork-release.yml) on every merge into `master`, versioned
`<upstream version>-fork.<timestamp>`. See [`fork/docs/releasing.md`](fork/docs/releasing.md).

### Backend entry

[`fork/background/mod.rs`](fork/background/mod.rs) — the fork's Rust module, compiled as part of the main binary.
`init()` starts every fork backend service; new ones are registered there, never in upstream files.

### Dock wheel bridge

[`fork/background/wheel_bridge.rs`](fork/background/wheel_bridge.rs) +
[`fork/weg/wheelBridge.ts`](fork/weg/wheelBridge.ts) — Windows only sends the mouse wheel to the hovered window when
_Scroll inactive windows when hovering over them_ is on, and the dock is never the focused window. A low-level mouse
hook catches the wheel over the dock (swallowing it, so the focused app doesn't scroll too) and forwards it to the dock
webview, which replays it as a normal DOM `wheel` event. Works with that Windows setting on or off. Transparent dock
areas are click-through, so the wheel over them still reaches the apps behind.

### App volume

[`fork/weg/appVolume.svelte.ts`](fork/weg/appVolume.svelte.ts) — on dock app items:

- **Scroll** changes the app volume in 5% steps.
- **Ctrl+click** mutes / unmutes the app (the normal click action is skipped).
- The **tooltip** shows `Volume: N%`, `Muted` or `No audio session` while doing so, until the pointer leaves.

### UAC prompts

[`fork/background/uac_prompt.rs`](fork/background/uac_prompt.rs) — when the program asking for elevation isn't in the
foreground (e.g. an installer started by winget in a terminal), Windows parks the UAC prompt as a flashing taskbar
button instead of showing it. Seelen UI hides the native taskbar and its dock skips `consent.exe` (it runs as SYSTEM),
so the prompt was unreachable. Any `consent.exe` window that appears on the user's desktop is a parked prompt (normal
ones live on the secure desktop), so it is brought to the front and the prompt shows as usual.

Restoring the window is not enough, and restoring it through the service is refused outright: a High integrity caller
still may not touch a SYSTEM window. Activation is the whole job, so the module runs a ladder, judging every step by
re-reading the real foreground window instead of trusting a call's return value:

1. `SwitchToThisWindow` — what the native taskbar button itself calls; needs no foreground permission and crosses
   integrity levels.
2. `SvcAction::SetForeground` through the elevated service — for when Seelen is not allowed to activate it.

A `SvcAction::ShowWindowAsync(SW_RESTORE)` is sent first, but only as a non-fatal hint. A prompt that can't be activated
is retried a few times and then left alone until it is shown again, so the user can switch away from it without it
coming back.

No upstream hook: it reuses the existing `SvcAction`s and the platform layer. More prompt hosts can be added to
`PROMPT_HOSTS`; they are only matched when installed under `%SystemRoot%\System32`.
