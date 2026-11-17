# weftos-fsutil

Std-only durable file replacement for WeftOS. [`write_atomic`] writes bytes to a
uniquely named sibling temp file, fsyncs it, renames it over the target and (on
unix) fsyncs the parent directory. No dependencies, no unsafe code, builds for
`wasm32-wasip1`.

Part of [WeftOS](https://github.com/WeftOS/weftos).
