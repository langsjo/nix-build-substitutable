//! Asks Nix what is missing from the store, using its typed daemon protocol.

use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use harmonia_protocol::daemon_wire::types2::QueryMissingResult;
use harmonia_protocol::types::DaemonStore;
use harmonia_store_derivation::derived_path::DerivedPath;
use harmonia_store_path::StoreDir;
use harmonia_store_remote::DaemonClientBuilder;
use tokio::process::Command;

/// Runs Nix's `queryMissing`, the computation behind `nix build --dry-run`.
///
/// Talks to a `nix daemon --stdio --process-ops` child instead of the daemon socket.
/// The child reads the user's Nix config and forwards it like any Nix CLI command does, and
/// also works on single-user installs that have no daemon. `--process-ops` is what makes the
/// child handle the operations itself; Nix gates it behind `mounted-ssh-store`.
pub async fn query_missing(
    store_dir: &StoreDir,
    targets: &[DerivedPath],
    settings: &[String],
) -> Result<QueryMissingResult> {
    let mut daemon = Command::new("nix")
        .args(["daemon", "--stdio", "--process-ops"])
        .args(["--extra-experimental-features", "mounted-ssh-store"])
        .args(settings)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawning `nix daemon`")?;
    let from_daemon = daemon.stdout.take().context("daemon stdout")?;
    let to_daemon = daemon.stdin.take().context("daemon stdin")?;

    let mut client = DaemonClientBuilder::new()
        .set_store_dir(store_dir)
        .connect(from_daemon, to_daemon)
        .await
        .map_err(|error| anyhow!("connecting to `nix daemon`: {error:?}"))?;
    client
        .query_missing(targets)
        .await
        .map_err(|error| anyhow!("querying missing paths: {error:?}"))
}
