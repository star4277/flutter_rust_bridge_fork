//! A thin wrapper over the `flutter run --machine` daemon protocol.
//!
//! The protocol is line based: every line on stdout is a JSON array holding one
//! object, which is either an event (`{"event": ..., "params": ...}`) or a
//! response to a request we sent (`{"id": ..., "result"/"error": ...}`).
//! Requests go to stdin in the same shape.
//!
//! Ref: `flutter_tools`' `lib/src/commands/daemon.dart` (`AppDomain`).

use crate::library::commands::fvm::command_arg_maybe_fvm;
use crate::misc::FvmInstallMode;
use anyhow::{bail, Context, Result};
use log::{debug, warn};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

/// Interesting things the daemon told us. Uninteresting events are dropped.
#[derive(Debug, Clone)]
pub(crate) enum DaemonEvent {
    /// The app is registered and has an id we can send commands for.
    AppStart { app_id: String },
    /// The Dart VM Service is listening. Carries what a debugger would attach to.
    DebugPort { ws_uri: String },
    /// The app finished starting up.
    AppStarted,
    /// The app is gone.
    AppStop,
    /// A log line from the app.
    Log { text: String, is_error: bool },
    /// The `flutter` process itself exited.
    ProcessExited,
}

#[derive(Deserialize)]
struct RawEvent {
    event: String,
    #[serde(default)]
    params: Value,
}

#[derive(Deserialize)]
struct RawResponse {
    id: i64,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    error: Option<Value>,
}

/// A running `flutter run --machine` process.
pub(crate) struct FlutterDaemon {
    child: Child,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    /// Responses are matched to requests by id. Kept as a single queue because
    /// we only ever have one request outstanding.
    responses: Receiver<RawResponse>,
    app_id: Option<String>,
}

impl FlutterDaemon {
    /// Spawn `flutter run --machine` and start pumping its stdout into `events`.
    pub(crate) fn spawn(
        dart_root: &Path,
        extra_args: &[String],
        fvm_install_mode: FvmInstallMode,
        events: Sender<DaemonEvent>,
    ) -> Result<Self> {
        // Honor `.fvmrc` like every other Flutter invocation in this crate,
        // otherwise we would silently run a different Flutter than `generate`.
        let mut argv: Vec<String> = Vec::new();
        match command_arg_maybe_fvm(Some(dart_root), fvm_install_mode) {
            // `fvm flutter ...` — the subcommand is always the plain name, and
            // `fvm` itself is a real executable so it resolves on every platform.
            Some(fvm) => argv.extend([fvm, "flutter".to_owned()]),
            // Invoked directly, so it must be the actual file name: on Windows
            // `flutter` is a batch file and `Command` will not find it without
            // the extension.
            None => argv.push(direct_flutter_binary().to_owned()),
        }
        argv.extend(["run".to_owned(), "--machine".to_owned()]);
        argv.extend(extra_args.iter().cloned());

        debug!("spawning {}", argv.join(" "));

        let (program, args) = argv.split_first().context("empty command")?;
        let mut child = Command::new(program)
            .args(args)
            .current_dir(dart_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Leave stderr inherited: Flutter's build errors go there and the
            // user needs to see them verbatim.
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("Fail to spawn `{}`. Is Flutter on PATH?", argv.join(" ")))?;

        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;

        let (response_tx, response_rx) = std::sync::mpsc::channel();
        thread::spawn(move || pump_stdout(BufReader::new(stdout), events, response_tx));

        Ok(Self {
            child,
            stdin: Mutex::new(stdin),
            next_id: AtomicI64::new(0),
            responses: response_rx,
            app_id: None,
        })
    }

    pub(crate) fn set_app_id(&mut self, app_id: String) {
        self.app_id = Some(app_id);
    }

    pub(crate) fn app_id(&self) -> Option<&str> {
        self.app_id.as_deref()
    }

    /// Hot reload (`full_restart == false`) or Dart-level hot restart.
    ///
    /// Note neither reloads the Rust cdylib — that is what
    /// [`Self::stop_and_wait`] plus a fresh spawn is for.
    pub(crate) fn restart(&self, full_restart: bool, reason: &str) -> Result<()> {
        let app_id = self.app_id.as_ref().context("app_id not known yet")?;
        let id = self.send(
            "app.restart",
            json!({
                "appId": app_id,
                "fullRestart": full_restart,
                "pause": false,
                "reason": reason,
            }),
        )?;
        self.await_response(id, Duration::from_secs(120))?;
        Ok(())
    }

    /// Call a Flutter service extension, e.g. `ext.flutter.debugPaint`.
    ///
    /// This is how the interactive keys that toggle debug overlays are served;
    /// `flutter run` does the same thing when you press them.
    pub(crate) fn call_service_extension(&self, method: &str, params: Value) -> Result<Value> {
        let app_id = self.app_id.as_ref().context("app_id not known yet")?;
        let id = self.send(
            "app.callServiceExtension",
            json!({
                "appId": app_id,
                "methodName": method,
                "params": params,
            }),
        )?;
        self.await_response(id, Duration::from_secs(30))
    }

    /// Ask the app to stop, then wait for the `flutter` process to actually go
    /// away.
    ///
    /// Waiting matters: on Windows the cdylib stays locked until the process
    /// dies, so `cargo build` would fail with a sharing violation if we started
    /// building too early.
    pub(crate) fn stop_and_wait(&mut self, timeout: Duration) -> Result<()> {
        if let Some(app_id) = self.app_id.clone() {
            match self.send("app.stop", json!({"appId": app_id})) {
                Ok(id) => {
                    if let Err(e) = self.await_response(id, timeout) {
                        debug!("app.stop response not received ({e:?}), killing instead");
                    }
                }
                // Broken pipe means it is already on its way out.
                Err(e) => debug!("cannot send app.stop ({e:?}), killing instead"),
            }
        }

        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait()? {
                debug!("flutter process exited with {status:?}");
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }

        warn!("`flutter` did not exit within {timeout:?}, killing it");
        let _ = self.child.kill();
        let _ = self.child.wait();
        Ok(())
    }

    fn send(&self, method: &str, params: Value) -> Result<i64> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let line = json!([{"id": id, "method": method, "params": params}]);
        debug!("daemon <- {line}");

        let mut stdin = self.stdin.lock().unwrap();
        writeln!(stdin, "{line}")?;
        stdin.flush()?;
        Ok(id)
    }

    fn await_response(&self, id: i64, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("Timed out waiting for response to request {id}");
            }
            let response = self.responses.recv_timeout(remaining)?;
            if response.id != id {
                // A response we no longer care about (e.g. a timed-out
                // request). Skip it rather than fail.
                debug!("skipping stale response id={}", response.id);
                continue;
            }
            if let Some(error) = response.error {
                bail!("Daemon reported error for request {id}: {error}");
            }
            return Ok(response.result);
        }
    }
}

impl Drop for FlutterDaemon {
    fn drop(&mut self) {
        // Never leave an orphaned app behind, e.g. when `frb run` is aborted.
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn pump_stdout(reader: impl BufRead, events: Sender<DaemonEvent>, responses: Sender<RawResponse>) {
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        match parse_line(line) {
            Some(ParsedLine::Event(event)) => {
                if let Some(event) = translate_event(event) {
                    if events.send(event).is_err() {
                        return;
                    }
                }
            }
            Some(ParsedLine::Response(response)) => {
                if responses.send(response).is_err() {
                    return;
                }
            }
            None => {
                // Flutter interleaves plain human-readable output with the
                // protocol; pass it through so the user still sees it.
                println!("{line}");
            }
        }
    }

    let _ = events.send(DaemonEvent::ProcessExited);
}

enum ParsedLine {
    Event(RawEvent),
    Response(RawResponse),
}

/// Protocol lines are a JSON array holding exactly one object. Anything else is
/// ordinary console output.
fn parse_line(line: &str) -> Option<ParsedLine> {
    if !line.starts_with('[') {
        return None;
    }
    let items: Vec<Value> = serde_json::from_str(line).ok()?;
    let item = items.into_iter().next()?;

    if item.get("event").is_some() {
        return serde_json::from_value(item).ok().map(ParsedLine::Event);
    }
    if item.get("id").is_some() {
        return serde_json::from_value(item).ok().map(ParsedLine::Response);
    }
    None
}

fn translate_event(raw: RawEvent) -> Option<DaemonEvent> {
    let params = &raw.params;
    match raw.event.as_str() {
        "app.start" => Some(DaemonEvent::AppStart {
            app_id: params.get("appId")?.as_str()?.to_owned(),
        }),
        "app.debugPort" => Some(DaemonEvent::DebugPort {
            ws_uri: params.get("wsUri")?.as_str()?.to_owned(),
        }),
        "app.started" => Some(DaemonEvent::AppStarted),
        "app.stop" => Some(DaemonEvent::AppStop),
        "app.log" => Some(DaemonEvent::Log {
            text: params.get("log")?.as_str()?.to_owned(),
            is_error: params
                .get("error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        _ => None,
    }
}

fn direct_flutter_binary() -> &'static str {
    if cfg!(windows) {
        "flutter.bat"
    } else {
        "flutter"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_line_event() {
        let line = r#"[{"event":"app.start","params":{"appId":"abc","deviceId":"windows"}}]"#;
        let Some(ParsedLine::Event(raw)) = parse_line(line) else {
            panic!("expect event");
        };
        assert_eq!(raw.event, "app.start");
        assert!(matches!(
            translate_event(raw),
            Some(DaemonEvent::AppStart { app_id }) if app_id == "abc"
        ));
    }

    #[test]
    fn test_parse_line_debug_port() {
        let line = r#"[{"event":"app.debugPort","params":{"appId":"abc","port":52341,"wsUri":"ws://127.0.0.1:52341/AbCd=/ws"}}]"#;
        let Some(ParsedLine::Event(raw)) = parse_line(line) else {
            panic!("expect event");
        };
        assert!(matches!(
            translate_event(raw),
            Some(DaemonEvent::DebugPort { ws_uri }) if ws_uri.ends_with("/ws")
        ));
    }

    #[test]
    fn test_parse_line_response() {
        let line = r#"[{"id":3,"result":{"code":0}}]"#;
        let Some(ParsedLine::Response(response)) = parse_line(line) else {
            panic!("expect response");
        };
        assert_eq!(response.id, 3);
        assert!(response.error.is_none());
    }

    #[test]
    fn test_parse_line_response_error() {
        let line = r#"[{"id":4,"error":"something went wrong"}]"#;
        let Some(ParsedLine::Response(response)) = parse_line(line) else {
            panic!("expect response");
        };
        assert_eq!(response.id, 4);
        assert!(response.error.is_some());
    }

    #[test]
    fn test_parse_line_plain_output_is_not_protocol() {
        assert!(parse_line("Launching lib/main.dart on Windows...").is_none());
        assert!(parse_line("").is_none());
        // A JSON array of something unrecognized is not a protocol line either
        assert!(parse_line("[1, 2, 3]").is_none());
    }

    #[test]
    fn test_translate_event_ignores_uninteresting() {
        let raw = RawEvent {
            event: "daemon.connected".to_owned(),
            params: json!({"pid": 123}),
        };
        assert!(translate_event(raw).is_none());
    }

    #[test]
    fn test_translate_event_log() {
        let raw = RawEvent {
            event: "app.log".to_owned(),
            params: json!({"appId": "abc", "log": "hello", "error": true}),
        };
        assert!(matches!(
            translate_event(raw),
            Some(DaemonEvent::Log { text, is_error: true }) if text == "hello"
        ));
    }
}
