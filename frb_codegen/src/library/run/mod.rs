//! `frb run` — run the app and keep it in sync with Rust and config changes.
//!
//! The problem this solves: Flutter's hot reload and hot restart are both
//! Dart-level, so neither picks up a rebuilt Rust cdylib. Once a shared library
//! is mapped into the process the dynamic linker will not re-read it from disk,
//! so the only reliable way to pick up Rust changes is to restart the process.
//! This module watches for changes, decides the cheapest correct action, and
//! drives `flutter run --machine` accordingly.

pub(crate) mod change_kind;
pub(crate) mod daemon;
pub(crate) mod keys;
pub(crate) mod rebuild;
pub(crate) mod watcher;

use crate::codegen::config::internal_config::InternalConfig;
use crate::codegen::{Config, MetaConfig};
use crate::misc::FvmInstallMode;
use crate::run::change_kind::{classify_batch, ChangeAction, WatchPaths};
use crate::run::daemon::{DaemonEvent, FlutterDaemon};
use crate::run::keys::{spawn_key_reader, KeyCommand, ToggleState};
use crate::run::watcher::spawn_watcher;
use crate::utils::path_utils::{normalize_windows_unc_path, path_to_string};
use anyhow::{Context, Result};
use itertools::Itertools;
use log::{debug, warn};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::Duration;
use std::{env, thread};

/// How long to wait for the app to shut down before killing it.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Options for [`run`], mirroring the CLI arguments.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Path to a codegen YAML config file, if not auto-detected.
    pub config_file: Option<String>,
    /// Device id forwarded to `flutter run -d`.
    pub device_id: Option<String>,
    /// Extra arguments forwarded verbatim to `flutter run`.
    pub flutter_args: Vec<String>,
    /// Skip the `cargo check` gate before restarting.
    pub no_check: bool,
    pub fvm_install_mode: FvmInstallMode,
}

/// Anything that can make us act. Merged into one channel so the main loop has a
/// single place to block.
enum Event {
    Daemon(DaemonEvent),
    FsChange(Vec<PathBuf>),
    Key(KeyCommand),
}

/// Run the app until the user quits.
pub fn run(run_config: RunConfig) -> Result<()> {
    let mut state = State::load(&run_config)?;

    println!("Watching:");
    for path in state.watch_summary() {
        println!("  {path}");
    }
    keys::print_help();

    // One channel for the whole session, because the key reader blocks on stdin
    // and cannot be shut down between restarts.
    let (tx, rx) = std::sync::mpsc::channel();
    spawn_key_reader(key_sender(tx.clone()));

    loop {
        match run_once(&mut state, &run_config, &tx, &rx)? {
            Outcome::Restart => continue,
            Outcome::Finished => return Ok(()),
        }
    }
}

enum Outcome {
    /// The Rust library changed, so the process must be recreated.
    Restart,
    /// The app exited and we are done.
    Finished,
}

/// Config-derived state. Rebuilt whenever a config file changes, because config
/// controls where the inputs and outputs live.
struct State {
    watch: WatchPaths,
    rust_features: Option<Vec<String>>,
}

impl State {
    fn load(run_config: &RunConfig) -> Result<Self> {
        let config = load_codegen_config(run_config)?;
        let rust_features = config.rust_features.clone();
        let watch = compute_watch_paths(&config)?;
        debug!("watch paths: {watch:?}");
        Ok(Self {
            watch,
            rust_features,
        })
    }

    fn watch_summary(&self) -> Vec<String> {
        [
            self.watch.rust_crate_dir.join("src"),
            self.watch.dart_root.join("lib"),
        ]
        .iter()
        .filter_map(|p| path_to_string(p).ok())
        .map(|p| normalize_windows_unc_path(&p).to_owned())
        .chain(["codegen config files (flutter_rust_bridge.yaml, pubspec.yaml, ...)".to_owned()])
        .collect()
    }
}

/// One process lifetime: spawn, serve changes, and return why we stopped.
fn run_once(
    state: &mut State,
    run_config: &RunConfig,
    tx: &Sender<Event>,
    rx: &Receiver<Event>,
) -> Result<Outcome> {
    // Drop events that queued up while the previous process was being replaced,
    // so a key pressed during a rebuild does not fire against the new app.
    drain_pending(rx);

    let watcher = spawn_watcher(&state.watch, fs_change_sender(tx.clone()))?;
    let mut daemon = FlutterDaemon::spawn(
        &state.watch.dart_root,
        &flutter_args(run_config),
        run_config.fvm_install_mode,
        daemon_event_sender(tx.clone()),
    )?;

    let outcome = serve(state, run_config, &mut daemon, rx, tx);

    // Always tear the app down before returning, so the next spawn is not
    // fighting the previous one over the cdylib file lock.
    if let Err(e) = daemon.stop_and_wait(STOP_TIMEOUT) {
        warn!("Error while stopping the app: {e:?}");
    }
    // Stop watching only after the app is down, so changes during the rebuild
    // are not silently missed.
    drop(watcher);

    outcome
}

fn serve(
    state: &mut State,
    run_config: &RunConfig,
    daemon: &mut FlutterDaemon,
    rx: &Receiver<Event>,
    tx: &Sender<Event>,
) -> Result<Outcome> {
    let mut toggles = ToggleState::default();

    loop {
        let event = match rx.recv() {
            Ok(event) => event,
            // Both senders are gone, which only happens on shutdown.
            Err(_) => return Ok(Outcome::Finished),
        };

        match event {
            Event::Daemon(DaemonEvent::AppStart { app_id }) => {
                debug!("app started with id {app_id}");
                daemon.set_app_id(app_id);
            }
            Event::Daemon(DaemonEvent::DebugPort { ws_uri }) => {
                println!("Dart VM Service: {ws_uri}");
            }
            Event::Daemon(DaemonEvent::AppStarted) => {
                println!("App started. Press `h` for the key commands.");
            }
            Event::Daemon(DaemonEvent::Log { text, is_error }) => {
                if is_error {
                    eprint!("{text}");
                } else {
                    print!("{text}");
                }
            }
            Event::Daemon(DaemonEvent::AppStop | DaemonEvent::ProcessExited) => {
                println!("App exited.");
                return Ok(Outcome::Finished);
            }
            Event::Key(command) => {
                match handle_key(command, state, run_config, daemon, &mut toggles)? {
                    Some(outcome) => return Ok(outcome),
                    None => continue,
                }
            }
            Event::FsChange(paths) => {
                let paths = drain_extra_changes(paths, rx, tx);
                let action = classify_batch(&paths, &state.watch);
                debug!("classified {} change(s) as {action:?}", paths.len());

                match action {
                    ChangeAction::Ignore => {}
                    ChangeAction::HotReload => hot_reload(daemon),
                    ChangeAction::RestartRust { needs_codegen } => {
                        if prepare_restart(state, run_config, needs_codegen)? {
                            return Ok(Outcome::Restart);
                        }
                    }
                    ChangeAction::ReloadConfig => {
                        println!("Config changed, reloading...");
                        // Reload before rebuilding: the new config decides both
                        // what codegen reads and where it writes.
                        *state = State::load(run_config)?;
                        prepare_restart(state, run_config, true)?;
                        // Restart regardless of whether the rebuild succeeded:
                        // the watched paths may have moved, so this process's
                        // watcher is stale either way.
                        return Ok(Outcome::Restart);
                    }
                }
            }
        }
    }
}

/// Act on a key press. `Some(outcome)` ends this process's lifetime.
fn handle_key(
    command: KeyCommand,
    state: &State,
    run_config: &RunConfig,
    daemon: &FlutterDaemon,
    toggles: &mut ToggleState,
) -> Result<Option<Outcome>> {
    match command {
        KeyCommand::Help => keys::print_help(),
        KeyCommand::Quit => {
            println!("Quitting...");
            return Ok(Some(Outcome::Finished));
        }
        KeyCommand::HotReload => hot_reload(daemon),
        KeyCommand::HotRestart => {
            if daemon.app_id().is_none() {
                debug!("app not ready yet, skipping hot restart");
            } else {
                println!("Hot restarting (Dart only)...");
                if let Err(e) = daemon.restart(true, "frb run: manual hot restart") {
                    warn!("Hot restart failed: {e:?}");
                }
            }
        }
        KeyCommand::RebuildRust => {
            // The one key `flutter run` cannot offer, since it has no idea Rust
            // is involved.
            if prepare_restart(state, run_config, true)? {
                return Ok(Some(Outcome::Restart));
            }
        }
        _ => {
            if daemon.app_id().is_none() {
                debug!("app not ready yet, ignoring key");
            } else {
                toggles.handle(command, daemon);
            }
        }
    }
    Ok(None)
}

fn hot_reload(daemon: &FlutterDaemon) {
    if daemon.app_id().is_none() {
        debug!("app not ready yet, skipping hot reload");
        return;
    }
    println!("Hot reloading...");
    if let Err(e) = daemon.restart(false, "frb run: dart change") {
        warn!("Hot reload failed: {e:?}");
    }
}

/// Run codegen and the compile gate. Returns whether the process should be
/// restarted; `false` means the build failed and the old app is still usable.
fn prepare_restart(state: &State, run_config: &RunConfig, needs_codegen: bool) -> Result<bool> {
    println!("Rust changed, rebuilding...");

    let config_source = || load_codegen_config(run_config);
    let result = if run_config.no_check {
        if needs_codegen {
            rebuild::run_codegen(config_source()?, run_config.fvm_install_mode)
        } else {
            Ok(())
        }
    } else {
        rebuild::rebuild(
            &state.watch,
            &config_source,
            needs_codegen,
            state.rust_features.as_deref(),
            run_config.fvm_install_mode,
        )
    };

    match result {
        Ok(()) => {
            println!("Restarting the process to pick up the new Rust library...");
            Ok(true)
        }
        Err(e) => {
            // Keep the old app alive: a compile error should cost a few seconds,
            // not the whole session.
            eprintln!("Rebuild failed, keeping the current app running:\n{e:?}");
            Ok(false)
        }
    }
}

/// Collect changes that arrived while we were busy, so a burst of saves causes
/// one rebuild instead of several.
fn drain_extra_changes(
    mut paths: Vec<PathBuf>,
    rx: &Receiver<Event>,
    tx: &Sender<Event>,
) -> Vec<PathBuf> {
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Event::FsChange(more)) => paths.extend(more),
            // Daemon chatter is not worth reordering the queue for.
            Ok(Event::Daemon(_)) => {}
            // A key pressed mid-burst still deserves to run, so put it back
            // rather than dropping it on the floor.
            Ok(Event::Key(command)) => {
                let _ = tx.send(Event::Key(command));
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    paths.into_iter().unique().collect()
}

/// Throw away whatever queued up while the previous process was being replaced.
fn drain_pending(rx: &Receiver<Event>) {
    loop {
        match rx.try_recv() {
            Ok(_) => {}
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return,
        }
    }
}

fn fs_change_sender(tx: Sender<Event>) -> Sender<Vec<PathBuf>> {
    let (raw_tx, raw_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        while let Ok(paths) = raw_rx.recv() {
            if tx.send(Event::FsChange(paths)).is_err() {
                return;
            }
        }
    });
    raw_tx
}

fn key_sender(tx: Sender<Event>) -> Sender<KeyCommand> {
    let (raw_tx, raw_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        while let Ok(command) = raw_rx.recv() {
            if tx.send(Event::Key(command)).is_err() {
                return;
            }
        }
    });
    raw_tx
}

fn daemon_event_sender(tx: Sender<Event>) -> Sender<DaemonEvent> {
    let (raw_tx, raw_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        while let Ok(event) = raw_rx.recv() {
            if tx.send(Event::Daemon(event)).is_err() {
                return;
            }
        }
    });
    raw_tx
}

fn flutter_args(run_config: &RunConfig) -> Vec<String> {
    let mut ans = Vec::new();
    if let Some(device_id) = &run_config.device_id {
        ans.push("-d".to_owned());
        ans.push(device_id.clone());
    }
    ans.extend(run_config.flutter_args.iter().cloned());
    ans
}

fn load_codegen_config(run_config: &RunConfig) -> Result<Config> {
    if let Some(config_file) = &run_config.config_file {
        return Config::from_config_file(config_file)?
            .with_context(|| format!("Cannot find config_file {config_file}"));
    }
    Config::from_files_auto_option()?.context(
        "Cannot find any flutter_rust_bridge config. \
         Please run inside the Flutter package, or pass --config-file.",
    )
}

/// Derive what to watch from the codegen config, reusing the very same parsing
/// codegen does so the two never disagree about where things live.
fn compute_watch_paths(config: &Config) -> Result<WatchPaths> {
    let internal = InternalConfig::parse(config, &MetaConfig { watch: false })?;

    let base_dir = match config.base_dir.as_ref().map(std::fs::canonicalize) {
        Some(Ok(path)) => path,
        None | Some(Err(_)) => env::current_dir()?,
    };

    let rust_crate_dir = internal.polisher.rust_crate_dir.clone();

    let generated_paths = vec![
        internal.polisher.rust_output_path.clone(),
        internal.polisher.dart_output.join("frb_generated.dart"),
    ];

    Ok(WatchPaths {
        base_dir,
        rust_crate_dir,
        dart_root: internal.polisher.dart_root.clone(),
        generated_paths,
    })
}
