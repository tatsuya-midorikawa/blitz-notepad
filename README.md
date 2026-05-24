# blitz-notepad

`blitz-notepad` is a Rust implementation of a Notepad-compatible text editor core.
The executable is named `blitzpad`.

## Current Scope

- `blitzpad` binary target with a software-rendered Notepad-style window.
- `--help`, `--version`, `--dump-ui`, and `--smoke-window` command-line options.
- `--screenshot <path>` startup UI rendering for repeatable UI verification.
- Piece Table based editing with undo snapshots.
- Memory-mapped loading for files at or above 50 MiB through `memmap2`.
- UTF-8, UTF-8 BOM, UTF-16 LE/BE, and ANSI/Shift_JIS detection and conversion.
- CRLF, LF, and CR line-ending detection and save-time normalization.
- Chunked line index for fast line lookup and visible-line extraction.
- Notepad-compatible menu/status/font dialog state model for native UI integration.
- Clickable menu bar commands for available editor actions, with status messages for pending dialogs.
- System TrueType rendering that prefers Aptos and Yu Gothic UI, with platform UI font fallbacks.
- Settings persistence for word wrap, status bar, font, script, and zoom.

The platform-native drawing layer is intentionally isolated from the core model so
that the Windows Win32/DirectWrite/Direct2D and macOS CoreText/CoreGraphics frontends
can be added without rewriting document handling.

## Build

```sh
cargo build
```

## Run

```sh
cargo run --bin blitzpad
cargo run --bin blitzpad -- --dump-ui
cargo run --bin blitzpad -- --screenshot .ui-test-output/startup.png
cargo run --bin blitzpad -- path/to/file.txt
```

`cargo run --bin blitzpad` opens the editor window. `--dump-ui` prints the UI
model for debugging, `--smoke-window` renders the window briefly and exits, and
`--screenshot <path>` writes the startup UI to a PNG file for UI tests.

The current window supports clicking to move the caret, Unicode text input,
arrow/Home/End movement, Enter, Tab, Backspace, Delete, F5 time/date insertion,
menu commands for implemented actions, and Ctrl/Cmd shortcuts for Select All,
Undo, Cut, Copy, Paste, Save, New, and Zoom.

## Test

```sh
cargo test
```