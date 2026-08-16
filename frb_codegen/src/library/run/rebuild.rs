//! Re-run codegen and rebuild the Rust crate.

use crate::codegen;
use crate::codegen::{Config, MetaConfig};
use crate::library::commands::command_runner::{call_shell, ExecuteCommandOptions};
use crate::misc::FvmInstallMode;
use crate::run::change_kind::WatchPaths;
use crate::utils::path_utils::path_to_string;
use anyhow::{bail, Result};
use log::debug;
use std::path::Path;

/// Run codegen once, without the watch loop (we own the watching here).
pub(crate) fn run_codegen(config: Config, fvm_install_mode: FvmInstallMode) -> Result<()> {
    codegen::generate_with_fvm_install_mode(config, MetaConfig { watch: false }, fvm_install_mode)
}

/// Compile the crate to surface errors before we tear the app down.
///
/// `flutter run` would build it again anyway, but doing it here means a Rust
/// compile error costs a few seconds instead of a full stop-and-restart cycle,
/// and the second build hits the cargo cache.
pub(crate) fn cargo_check(rust_crate_dir: &Path, rust_features: Option<&[String]>) -> Result<()> {
    let mut args = vec![
        "cargo".to_owned(),
        "check".to_owned(),
        "--manifest-path".to_owned(),
        path_to_string(&rust_crate_dir.join("Cargo.toml"))?,
    ];
    if let Some(features) = rust_features {
        if !features.is_empty() {
            args.push("--features".to_owned());
            args.push(features.join(","));
        }
    }

    debug!("running {}", args.join(" "));

    let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    let output = call_shell(
        &args,
        Some(rust_crate_dir),
        Some(ExecuteCommandOptions {
            // Cargo already printed the diagnostics; a second dump as a warning
            // would only bury them.
            log_when_error: Some(false),
            ..Default::default()
        }),
    )?;

    if !output.status.success() {
        // Cargo writes diagnostics to stderr, and we capture rather than
        // inherit, so relay them or the user sees nothing.
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        bail!("`cargo check` failed; keeping the current app running");
    }
    Ok(())
}

/// Everything a Rust-side change needs before the app can be restarted.
pub(crate) fn rebuild(
    watch: &WatchPaths,
    config_source: &dyn Fn() -> Result<Config>,
    needs_codegen: bool,
    rust_features: Option<&[String]>,
    fvm_install_mode: FvmInstallMode,
) -> Result<()> {
    if needs_codegen {
        println!("Running code generation...");
        run_codegen(config_source()?, fvm_install_mode)?;
    }
    println!("Checking Rust...");
    cargo_check(&watch.rust_crate_dir, rust_features)
}
