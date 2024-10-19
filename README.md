# LSP-file-watcher-rust

A non-native file watcher implementation for [LSP](https://packagecontrol.io/packages/LSP) that enables support for the `workspace/didChangeWatchedFiles` LSP notification. Implemented in Rust using [notify](https://github.com/notify-rs/notify). It has smaller footprint compared to the upstream [LSP-file-watcher-chokidar](https://github.com/sublimelsp/LSP-file-watcher-chokidar) and does not require a Node.js runtime.

## Installation

* `cargo build --profile release`
* Move the built `chokidar` from `target/release` to project's root directory.
* Copy the entire directory to the packages diretory of Sublime (`Preferences` - `Browser Packages...`).
