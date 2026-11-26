# wartlog-viewer

The WartLog desktop viewer: Qt 6 Quick Controls with a Rust backend (cxx-qt 0.10). On Plasma it
uses the `org.kde.desktop` style when installed, so it follows the desktop theme; elsewhere it
uses Fusion.

It is not a workspace member because it needs Qt 6 development files. Build it on its own:

```sh
# Ubuntu / Debian: qt6-base-dev qt6-declarative-dev qml6-module-qtquick-controls
#                  qml6-module-qtquick-layouts qml6-module-qtquick-templates qml6-module-qtqml-workerscript
# Fedora:          qt6-qtbase-devel qt6-qtdeclarative-devel
cd crates/wartlog-viewer
QMAKE=$(command -v qmake6) cargo build --release
./target/release/wartlog-viewer
```

`wl setup` installs a `.desktop` entry when it finds `wartlog-viewer` next to `wl`.

Environment:

| Variable | Meaning |
|---|---|
| `WARTLOG_STORE` | store directory (default `$XDG_DATA_HOME/wartlog`) |
| `WARTLOG_SOCKET` | daemon API socket (default `$XDG_RUNTIME_DIR/wartlog/api.sock`) |
| `QT_QUICK_CONTROLS_STYLE` | override the style (`Fusion`, `org.kde.desktop`, ...) |
| `WARTLOG_SCREENSHOT`, `WARTLOG_SCREENSHOT_STEP` | for checks: drive the UI (`details`, `context`, `trace`, `templates`, `error`, `filter`), save a PNG, quit |

History queries, details, context, templates and traces read the store files directly, so they
work without the daemon. Live tail and source changes need `wartlogd`.

Keys: `Ctrl+K` query bar, `Ctrl+L` live tail, `/` find in results, `Esc` clear selection, `F5` rerun,
`Tab` or `Enter` accept a completion, `Ctrl+Space` show completions.
