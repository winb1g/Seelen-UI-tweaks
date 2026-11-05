<!-- [fork] GitHub shows .github/README.md instead of the root README.md, so the fork gets its own landing page without
touching upstream's README. -->

# Seelen UI Tweaks

**Unofficial fork of [Seelen UI](https://github.com/eythaann/Seelen-UI)** with small fixes and quality-of-life tweaks.
It tracks upstream closely (merged monthly) and keeps every change isolated, so it's always upstream plus a thin layer.

All credit for Seelen UI goes to [eythaann](https://github.com/eythaann) and its contributors. Please report bugs that
also happen in official Seelen UI [upstream](https://github.com/eythaann/Seelen-UI/issues), not here.

## What's different

See **[FORK.md](../FORK.md#features)** for the list of changes. Highlights:

- **Dock: per-app volume.** Scroll over a dock app to change its volume, ctrl+click to mute it.
- **Dock: mouse wheel works** without Windows' _Scroll inactive windows_ setting.
- **Battery: charging bolt shows up** reliably when the charger is plugged in (toolbar and dock).
- **Self-updates from this fork** (every merge is a release).

## Install

Download `Seelen.UI.Tweaks_<version>_x64-setup.exe` from the
**[latest release](https://github.com/winb1g/Seelen-UI-tweaks/releases/latest)** (Windows 10/11 x64). It replaces an
official install and keeps your settings; updates come through Settings. Details:
[installing & updating](../fork/docs/updating.md).

## Docs

- [FORK.md](../FORK.md): rules, upstream hooks, features
- [Releasing](../fork/docs/releasing.md) · [CI](../fork/docs/ci.md)
- Upstream's [README](../README.md) and [documentation](../documentation/)

## License

[AGPL-3.0](../LICENSE), same as upstream.
