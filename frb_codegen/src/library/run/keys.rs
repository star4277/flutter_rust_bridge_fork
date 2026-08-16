//! Interactive keyboard commands, mirroring what `flutter run` offers.
//!
//! `flutter run --machine` speaks JSON on stdin, which means the usual
//! interactive keys are gone: pressing `r` there does nothing. Since we own the
//! terminal we read the keys ourselves and translate them into daemon requests,
//! so the familiar keys keep working.

use crate::run::daemon::FlutterDaemon;
use anyhow::Result;
use log::debug;
use serde_json::{json, Value};
use std::sync::mpsc::Sender;
use std::thread;

/// What the user asked for by pressing a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyCommand {
    /// `r` — Dart hot reload.
    HotReload,
    /// `R` — Dart hot restart. Note this still does not reload Rust; `F` does.
    HotRestart,
    /// `F` — our addition: rebuild Rust and restart the process.
    RebuildRust,
    /// `q` — quit.
    Quit,
    /// `h` — print the help.
    Help,
    /// `p` — toggle the debug paint overlay.
    ToggleDebugPaint,
    /// `o` — cycle the platform between Android and iOS.
    TogglePlatform,
    /// `b` — toggle between light and dark theme.
    ToggleBrightness,
    /// `P` — toggle the performance overlay.
    TogglePerformanceOverlay,
    /// `w` — dump the widget hierarchy.
    DumpWidgetHierarchy,
}

impl KeyCommand {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            b'r' => Some(Self::HotReload),
            b'R' => Some(Self::HotRestart),
            b'F' => Some(Self::RebuildRust),
            // Ctrl-C is handled by the OS, but `q` is what `flutter run` uses.
            b'q' | b'Q' => Some(Self::Quit),
            b'h' | b'H' | b'?' => Some(Self::Help),
            b'p' => Some(Self::ToggleDebugPaint),
            b'o' | b'O' => Some(Self::TogglePlatform),
            b'b' | b'B' => Some(Self::ToggleBrightness),
            b'P' => Some(Self::TogglePerformanceOverlay),
            b'w' | b'W' => Some(Self::DumpWidgetHierarchy),
            _ => None,
        }
    }
}

/// Read keys and forward the recognized ones to `tx`.
///
/// Returns without doing anything when stdin is not a terminal (a CI run, or
/// output piped elsewhere), where raw-mode reads would fail or steal input.
pub(crate) fn spawn_key_reader(tx: Sender<KeyCommand>) {
    let term = console::Term::stdout();
    if !term.is_term() {
        debug!("stdin is not a terminal, interactive keys disabled");
        return;
    }

    thread::spawn(move || {
        loop {
            // Reads a single key without waiting for Enter.
            let Ok(key) = term.read_char() else {
                debug!("cannot read key, stopping the key reader");
                return;
            };
            let mut buf = [0u8; 4];
            let bytes = key.encode_utf8(&mut buf).as_bytes();
            let Some(&byte) = bytes.first() else { continue };
            if bytes.len() > 1 {
                // Multi-byte input is never one of our keys.
                continue;
            }

            if let Some(command) = KeyCommand::from_byte(byte) {
                if tx.send(command).is_err() {
                    return;
                }
            }
        }
    });
}

pub(crate) fn print_help() {
    println!(
        "
Flutter run key commands:
  r  Hot reload (Dart only)
  R  Hot restart (Dart only, does NOT reload Rust)
  F  Rebuild Rust and restart the process
  p  Toggle the debug paint overlay
  P  Toggle the performance overlay
  o  Toggle between Android and iOS platform
  b  Toggle light/dark brightness
  w  Dump the widget hierarchy
  h  Show this help
  q  Quit
"
    );
}

/// Toggles need the new value sent explicitly, so remember what we last set.
#[derive(Debug, Default)]
pub(crate) struct ToggleState {
    debug_paint: bool,
    performance_overlay: bool,
    /// `None` until we have asked the app which platform it currently uses.
    platform: Option<String>,
    /// `None` until we have asked the app for its current brightness.
    brightness: Option<String>,
}

impl ToggleState {
    /// Run a service-extension backed key. Errors are reported but never fatal:
    /// a failed overlay toggle should not take the session down.
    pub(crate) fn handle(&mut self, command: KeyCommand, daemon: &FlutterDaemon) {
        let result = match command {
            KeyCommand::ToggleDebugPaint => {
                self.debug_paint = !self.debug_paint;
                call_bool(daemon, "ext.flutter.debugPaint", self.debug_paint)
            }
            KeyCommand::TogglePerformanceOverlay => {
                self.performance_overlay = !self.performance_overlay;
                call_bool(
                    daemon,
                    "ext.flutter.showPerformanceOverlay",
                    self.performance_overlay,
                )
            }
            KeyCommand::TogglePlatform => self.toggle_platform(daemon),
            KeyCommand::ToggleBrightness => self.toggle_brightness(daemon),
            KeyCommand::DumpWidgetHierarchy => daemon
                .call_service_extension("ext.flutter.debugDumpApp", json!({}))
                .map(|result| {
                    // The dump comes back in the response rather than the log
                    // stream, so print it ourselves.
                    if let Some(data) = result.get("data").and_then(Value::as_str) {
                        println!("{data}");
                    }
                }),
            // Not a service extension; the caller deals with these.
            _ => return,
        };

        if let Err(e) = result {
            eprintln!("Key command failed: {e}");
        }
    }

    fn toggle_platform(&mut self, daemon: &FlutterDaemon) -> Result<()> {
        // Ask before setting: the app may have been launched on either platform,
        // and guessing would flip it the wrong way on the first press.
        let current = match &self.platform {
            Some(platform) => platform.clone(),
            None => read_string(daemon, "ext.flutter.platformOverride", "value")?,
        };
        let next = if current == "iOS" { "android" } else { "iOS" };

        let result = daemon
            .call_service_extension("ext.flutter.platformOverride", json!({"value": next}))?;
        let applied = (result.get("value").and_then(Value::as_str))
            .unwrap_or(next)
            .to_owned();
        println!("Platform: {applied}");
        self.platform = Some(applied);
        Ok(())
    }

    fn toggle_brightness(&mut self, daemon: &FlutterDaemon) -> Result<()> {
        let current = match &self.brightness {
            Some(brightness) => brightness.clone(),
            None => read_string(daemon, "ext.flutter.brightnessOverride", "value")?,
        };
        let next = if current == "Brightness.light" {
            "Brightness.dark"
        } else {
            "Brightness.light"
        };

        let result = daemon
            .call_service_extension("ext.flutter.brightnessOverride", json!({"value": next}))?;
        let applied = (result.get("value").and_then(Value::as_str))
            .unwrap_or(next)
            .to_owned();
        println!("Brightness: {applied}");
        self.brightness = Some(applied);
        Ok(())
    }
}

fn call_bool(daemon: &FlutterDaemon, method: &str, value: bool) -> Result<()> {
    // Flutter's service extensions take their booleans as strings.
    daemon.call_service_extension(method, json!({"enabled": value.to_string()}))?;
    println!("{method}: {value}");
    Ok(())
}

/// Calling an override extension with no arguments reads the current value.
fn read_string(daemon: &FlutterDaemon, method: &str, field: &str) -> Result<String> {
    let result = daemon.call_service_extension(method, json!({}))?;
    Ok((result.get(field).and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_mapping_matches_flutter_run() {
        assert_eq!(KeyCommand::from_byte(b'r'), Some(KeyCommand::HotReload));
        assert_eq!(KeyCommand::from_byte(b'R'), Some(KeyCommand::HotRestart));
        assert_eq!(KeyCommand::from_byte(b'q'), Some(KeyCommand::Quit));
        assert_eq!(KeyCommand::from_byte(b'h'), Some(KeyCommand::Help));
    }

    #[test]
    fn test_hot_reload_and_restart_are_case_sensitive() {
        // `flutter run` distinguishes these two, so we must not fold the case.
        assert_ne!(
            KeyCommand::from_byte(b'r'),
            KeyCommand::from_byte(b'R'),
            "r and R must stay distinct"
        );
        // Same for the two overlay toggles.
        assert_ne!(
            KeyCommand::from_byte(b'p'),
            KeyCommand::from_byte(b'P'),
            "p and P must stay distinct"
        );
    }

    #[test]
    fn test_rebuild_rust_key() {
        assert_eq!(KeyCommand::from_byte(b'F'), Some(KeyCommand::RebuildRust));
        // Lowercase `f` is unassigned in `flutter run`; leave it that way rather
        // than surprise anyone with a full rebuild.
        assert_eq!(KeyCommand::from_byte(b'f'), None);
    }

    #[test]
    fn test_unknown_keys_ignored() {
        for byte in [b'x', b'1', b' ', b'\n', b'\t'] {
            assert_eq!(KeyCommand::from_byte(byte), None, "byte {byte}");
        }
    }

    #[test]
    fn test_toggle_state_starts_disabled() {
        // The overlays start off, so the first press must turn them on.
        let state = ToggleState::default();
        assert!(!state.debug_paint);
        assert!(!state.performance_overlay);
        assert!(state.platform.is_none());
        assert!(state.brightness.is_none());
    }
}
