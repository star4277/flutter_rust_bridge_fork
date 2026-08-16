//! Watch the file system for changes that should re-run the app.

use crate::run::change_kind::{WatchPaths, CONFIG_FILE_NAMES};
use anyhow::Result;
use itertools::Itertools;
use log::debug;
use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

/// Held to keep watching; dropping it stops the watch.
pub(crate) struct FsWatcher {
    _debouncer: Debouncer<RecommendedWatcher>,
}

/// Start watching everything that can affect the built app, and send the
/// changed paths to `tx`.
///
/// Beyond Rust sources this covers the codegen config files: editing
/// `flutter_rust_bridge.yaml` can change the generated output just as much as
/// editing Rust does, and it can even move the input and output paths.
pub(crate) fn spawn_watcher(watch: &WatchPaths, tx: Sender<Vec<PathBuf>>) -> Result<FsWatcher> {
    let mut debouncer = new_debouncer(
        // Small enough to feel instant, large enough to coalesce the bursts
        // editors emit when saving.
        Duration::from_millis(300),
        move |event: DebounceEventResult| {
            if let Ok(events) = event {
                let paths = events.into_iter().map(|e| e.path).collect_vec();
                if !paths.is_empty() {
                    debug!("fs change: {paths:?}");
                    // The receiver going away just means we are shutting down.
                    let _ = tx.send(paths);
                }
            }
        },
    )?;

    let mut watched = 0usize;

    for dir in watch_dirs(watch) {
        if dir.is_dir() {
            debouncer.watcher().watch(&dir, RecursiveMode::Recursive)?;
            watched += 1;
        }
    }

    // Config files are watched individually rather than by watching their
    // parent directory, which would drown us in events from sibling files.
    for file in config_files(watch) {
        if file.is_file() {
            debouncer
                .watcher()
                .watch(&file, RecursiveMode::NonRecursive)?;
            watched += 1;
        }
    }

    debug!("watching {watched} path(s)");

    Ok(FsWatcher {
        _debouncer: debouncer,
    })
}

fn watch_dirs(watch: &WatchPaths) -> Vec<PathBuf> {
    vec![
        // The whole crate, not just the codegen input: a struct definition in a
        // non-input file can still change the generated code.
        watch.rust_crate_dir.join("src"),
        watch.dart_root.join("lib"),
    ]
}

/// Every path the config could be read from. Watched even when absent is
/// impossible with `notify`, so creating a config file later needs a restart of
/// `frb run` — noted in the docs.
fn config_files(watch: &WatchPaths) -> Vec<PathBuf> {
    let mut ans = Vec::new();
    for dir in [&watch.base_dir, &watch.dart_root, &watch.rust_crate_dir] {
        for name in CONFIG_FILE_NAMES {
            ans.push(dir.join(name));
        }
    }
    // Cargo.toml / build.rs are not directories, and live at the crate root
    // rather than under `src`, so they need explicit entries too.
    for name in ["Cargo.toml", "build.rs"] {
        ans.push(watch.rust_crate_dir.join(name));
    }
    ans.into_iter().unique().collect()
}
