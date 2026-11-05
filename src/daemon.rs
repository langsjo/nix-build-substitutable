//! Talks to Nix through its typed daemon protocol.

use std::pin::pin;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use harmonia_file_nar::{NarEvent, parse_nar};
use harmonia_protocol::daemon_wire::types2::QueryMissingResult;
use harmonia_protocol::types::DaemonStore;
use harmonia_store_derivation::derived_path::DerivedPath;
use harmonia_store_path::{StoreDir, StorePath};
use harmonia_store_remote::{DaemonClient, DaemonClientBuilder};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// A connection to the store selected by the user's Nix config, environment and `settings`.
///
/// Talks to a `nix daemon --stdio --process-ops` child instead of the daemon socket.
/// The child resolves the store and reads the user's Nix config like any Nix CLI command does,
/// and also works without a running daemon. `--process-ops` is what makes the child handle the
/// operations itself; Nix gates it behind `mounted-ssh-store`.
pub struct NixDaemon {
    client: DaemonClient<ChildStdout, ChildStdin>,
    _child: Child,
}

impl NixDaemon {
    pub async fn connect(store_dir: &StoreDir, settings: &[String]) -> Result<Self> {
        let mut child = Command::new("nix")
            .args(["daemon", "--stdio", "--process-ops"])
            .args(["--extra-experimental-features", "mounted-ssh-store"])
            .args(settings)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("spawning `nix daemon`")?;
        let from_daemon = child.stdout.take().context("daemon stdout")?;
        let to_daemon = child.stdin.take().context("daemon stdin")?;

        let client = DaemonClientBuilder::new()
            .set_store_dir(store_dir)
            .connect(from_daemon, to_daemon)
            .await
            .map_err(|error| anyhow!("connecting to `nix daemon`: {error:?}"))?;
        Ok(Self {
            client,
            _child: child,
        })
    }

    /// Runs Nix's `queryMissing`, the computation behind `nix build --dry-run`.
    pub async fn query_missing(&mut self, targets: &[DerivedPath]) -> Result<QueryMissingResult> {
        self.client
            .query_missing(targets)
            .await
            .map_err(|error| anyhow!("querying missing paths: {error:?}"))
    }

    /// Reads a store path that is a single file, such as a `.drv`, from any kind of store.
    pub async fn read_file(&mut self, path: &StorePath) -> Result<Vec<u8>> {
        let nar = self
            .client
            .nar_from_path(path)
            .await
            .map_err(|error| anyhow!("reading {path}: {error:?}"))?;
        let mut events = pin!(parse_nar(nar));

        let mut contents = Vec::new();
        match events.next().await {
            Some(Ok(NarEvent::File { mut reader, .. })) => {
                reader.read_to_end(&mut contents).await?;
            }
            Some(Err(error)) => return Err(error).context(format!("reading {path}")),
            _ => bail!("{path} is not a single file"),
        }
        // Reading to the end of the NAR leaves the connection ready for the next request.
        while let Some(event) = events.next().await {
            event.with_context(|| format!("reading {path}"))?;
        }
        Ok(contents)
    }
}
