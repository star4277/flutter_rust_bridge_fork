//! Classify a file system change into the action it should trigger.

use std::path::{Path, PathBuf};

/// All file names that [`crate::codegen::Config::from_files_auto_option`] probes,
/// in the same order. Keep in sync with `config_parser.rs`.
pub(crate) const CONFIG_FILE_NAMES: [&str; 7] = [
    ".flutter_rust_bridge.yml",
    ".flutter_rust_bridge.yaml",
    ".flutter_rust_bridge.json",
    "flutter_rust_bridge.yml",
    "flutter_rust_bridge.yaml",
    "flutter_rust_bridge.json",
    // The `flutter_rust_bridge:` section inside pubspec.yaml is a config source too
    "pubspec.yaml",
];

/// What a batch of file changes requires us to do.
///
/// Ordered by increasing cost, so a batch of mixed changes can be reduced with
/// [`Ord::max`] and the most expensive action wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ChangeAction {
    /// Nothing of interest changed (e.g. a file we generated ourselves).
    Ignore,
    /// Dart-only change: a plain hot reload suffices.
    HotReload,
    /// Rust changed: re-run codegen if needed, then restart the process.
    ///
    /// Restarting is unavoidable because an already-`dlopen`ed cdylib is never
    /// re-read from disk; see the design doc for the full reasoning.
    RestartRust { needs_codegen: bool },
    /// A codegen config file changed. Codegen inputs and output paths may both
    /// have moved, so config must be re-parsed and the watcher rebuilt.
    ReloadConfig,
}

/// Paths needed to classify changes. Derived from the parsed config, so it must
/// be recomputed whenever [`ChangeAction::ReloadConfig`] fires.
#[derive(Debug, Clone)]
pub(crate) struct WatchPaths {
    /// Directory the config files are resolved relative to.
    pub base_dir: PathBuf,
    pub rust_crate_dir: PathBuf,
    pub dart_root: PathBuf,
    /// Files we generate ourselves. Changes here must never trigger a rebuild,
    /// otherwise codegen would loop forever.
    pub generated_paths: Vec<PathBuf>,
}

pub(crate) fn classify_batch(paths: &[PathBuf], watch: &WatchPaths) -> ChangeAction {
    (paths.iter())
        .map(|path| classify_one(path, watch))
        .max()
        .unwrap_or(ChangeAction::Ignore)
}

pub(crate) fn classify_one(path: &Path, watch: &WatchPaths) -> ChangeAction {
    if is_generated(path, watch) {
        return ChangeAction::Ignore;
    }
    if is_config_file(path, watch) {
        return ChangeAction::ReloadConfig;
    }
    if let Some(action) = classify_rust(path, watch) {
        return action;
    }
    if is_dart_source(path, watch) {
        return ChangeAction::HotReload;
    }
    ChangeAction::Ignore
}

/// Generated outputs, matched by the same patterns `CLAUDE.md` forbids editing
/// by hand, plus the exact output paths codegen reported.
fn is_generated(path: &Path, watch: &WatchPaths) -> bool {
    if watch.generated_paths.iter().any(|p| paths_eq(p, path)) {
        return true;
    }
    let Some(name) = file_name(path) else {
        return false;
    };
    name.starts_with("frb_generated")
        || name.ends_with(".freezed.dart")
        || name.ends_with(".g.dart")
}

fn is_config_file(path: &Path, watch: &WatchPaths) -> bool {
    let Some(name) = file_name(path) else {
        return false;
    };
    if !CONFIG_FILE_NAMES.contains(&name.as_str()) {
        return false;
    }
    // Only the ones next to where config is resolved from. A `pubspec.yaml`
    // deeper in the tree (e.g. in an example app) is somebody else's.
    path.parent().is_some_and(|parent| {
        paths_eq(parent, &watch.base_dir) || paths_eq(parent, &watch.dart_root)
    })
}

fn classify_rust(path: &Path, watch: &WatchPaths) -> Option<ChangeAction> {
    let name = file_name(path)?;

    // Cargo.toml / build.rs change what gets compiled but are not parsed by
    // codegen, so a rebuild is enough.
    if (name == "Cargo.toml" || name == "Cargo.lock" || name == "build.rs")
        && path.starts_with(&watch.rust_crate_dir)
    {
        return Some(ChangeAction::RestartRust {
            needs_codegen: false,
        });
    }

    if path.extension()?.to_str()? != "rs" {
        return None;
    }
    if !path.starts_with(&watch.rust_crate_dir) {
        return None;
    }

    // Any Rust file, not just the codegen input: a struct definition in a
    // non-input file can still change the generated code. This matches how
    // `codegen`'s own watcher scopes itself.
    Some(ChangeAction::RestartRust {
        needs_codegen: true,
    })
}

fn is_dart_source(path: &Path, watch: &WatchPaths) -> bool {
    path.extension().and_then(|x| x.to_str()) == Some("dart")
        && path.starts_with(watch.dart_root.join("lib"))
}

fn file_name(path: &Path) -> Option<String> {
    Some(path.file_name()?.to_str()?.to_owned())
}

/// Compare paths tolerating the `\\?\` prefix that canonicalization adds on
/// Windows, since watcher events and config-derived paths disagree about it.
fn paths_eq(a: &Path, b: &Path) -> bool {
    fn normalize(path: &Path) -> String {
        let text = path.to_string_lossy();
        let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
        text.replace('\\', "/").trim_end_matches('/').to_owned()
    }
    normalize(a) == normalize(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch_paths() -> WatchPaths {
        let root = PathBuf::from("/proj");
        WatchPaths {
            base_dir: root.clone(),
            rust_crate_dir: root.join("rust"),
            dart_root: root.clone(),
            generated_paths: vec![
                root.join("rust").join("src").join("frb_generated.rs"),
                root.join("lib")
                    .join("src")
                    .join("rust")
                    .join("frb_generated.dart"),
            ],
        }
    }

    fn classify(path: &str) -> ChangeAction {
        classify_one(&PathBuf::from(path), &watch_paths())
    }

    #[test]
    fn test_classify_config_files() {
        // Every name `Config::from_config_files` probes must be recognized,
        // since editing any of them can change the generated output.
        for name in [
            ".flutter_rust_bridge.yml",
            ".flutter_rust_bridge.yaml",
            ".flutter_rust_bridge.json",
            "flutter_rust_bridge.yml",
            "flutter_rust_bridge.yaml",
            "flutter_rust_bridge.json",
            "pubspec.yaml",
        ] {
            assert_eq!(
                classify(&format!("/proj/{name}")),
                ChangeAction::ReloadConfig,
                "{name} should be treated as a config change"
            );
        }
    }

    #[test]
    fn test_classify_unrelated_pubspec_is_ignored() {
        // A pubspec belonging to some nested package is not our config.
        assert_eq!(classify("/proj/example/pubspec.yaml"), ChangeAction::Ignore);
    }

    #[test]
    fn test_classify_rust_inside_codegen_input_needs_codegen() {
        assert_eq!(
            classify("/proj/rust/src/api/minimal.rs"),
            ChangeAction::RestartRust {
                needs_codegen: true
            }
        );
    }

    #[test]
    fn test_classify_rust_outside_codegen_input_still_needs_codegen() {
        // A struct definition outside `rust_input` can still change the
        // generated code, so every Rust file re-runs codegen.
        assert_eq!(
            classify("/proj/rust/src/helper.rs"),
            ChangeAction::RestartRust {
                needs_codegen: true
            }
        );
    }

    #[test]
    fn test_classify_manifest_skips_codegen() {
        for name in ["Cargo.toml", "Cargo.lock", "build.rs"] {
            assert_eq!(
                classify(&format!("/proj/rust/{name}")),
                ChangeAction::RestartRust {
                    needs_codegen: false
                },
                "{name} should rebuild without re-running codegen"
            );
        }
    }

    #[test]
    fn test_classify_dart_source_hot_reloads() {
        assert_eq!(classify("/proj/lib/main.dart"), ChangeAction::HotReload);
    }

    #[test]
    fn test_classify_generated_files_are_ignored() {
        // Otherwise codegen writing its own output would trigger another round.
        for path in [
            "/proj/rust/src/frb_generated.rs",
            "/proj/lib/src/rust/frb_generated.dart",
            "/proj/lib/src/rust/frb_generated.io.dart",
            "/proj/lib/model.freezed.dart",
            "/proj/lib/model.g.dart",
        ] {
            assert_eq!(classify(path), ChangeAction::Ignore, "{path}");
        }
    }

    #[test]
    fn test_classify_batch_takes_most_expensive() {
        let watch = watch_paths();
        let paths = vec![
            PathBuf::from("/proj/lib/main.dart"),
            PathBuf::from("/proj/rust/src/api/minimal.rs"),
        ];
        assert_eq!(
            classify_batch(&paths, &watch),
            ChangeAction::RestartRust {
                needs_codegen: true
            }
        );

        // Config outranks Rust, because it can move where Rust even lives.
        let paths = vec![
            PathBuf::from("/proj/rust/src/api/minimal.rs"),
            PathBuf::from("/proj/flutter_rust_bridge.yaml"),
        ];
        assert_eq!(classify_batch(&paths, &watch), ChangeAction::ReloadConfig);
    }

    #[test]
    fn test_classify_batch_empty_is_ignore() {
        assert_eq!(classify_batch(&[], &watch_paths()), ChangeAction::Ignore);
    }

    #[test]
    fn test_action_ordering() {
        // `classify_batch` relies on this ordering to pick the winner.
        assert!(ChangeAction::Ignore < ChangeAction::HotReload);
        assert!(
            ChangeAction::HotReload
                < ChangeAction::RestartRust {
                    needs_codegen: false
                }
        );
        assert!(
            ChangeAction::RestartRust {
                needs_codegen: true
            } < ChangeAction::ReloadConfig
        );
    }

    #[test]
    fn test_paths_eq_tolerates_windows_prefix_and_separators() {
        assert!(paths_eq(
            &PathBuf::from(r"\\?\C:\proj"),
            &PathBuf::from(r"C:\proj")
        ));
        assert!(paths_eq(&PathBuf::from("/proj/"), &PathBuf::from("/proj")));
        assert!(!paths_eq(&PathBuf::from("/proj"), &PathBuf::from("/other")));
    }
}
