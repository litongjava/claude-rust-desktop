# Local patch to Crossterm 0.28.1

The source and MIT license are from the crates.io release. The local patch
returns terminal read closure, EOF and permanent read errors from the Unix
event source. Cursor queries also propagate the error so a pending report
cannot keep a disconnected terminal alive. The CLI tests exercise both an
active cursor query and a normal event poll on a PTY.

The patch changes these three files.

- `src/event/source/unix/mio.rs`
- `src/event/source/unix/tty.rs`
- `src/cursor/sys/unix.rs`

Every other vendored file uses the crates.io 0.28.1 source with LF line endings.
`Cargo.toml` includes the runtime portion of the release’s `Cargo.toml.orig`,
omitting its development dependencies and example targets.

Remove the workspace patch when the dependency handles these paths itself.
