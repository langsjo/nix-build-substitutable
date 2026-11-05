//! Builds only the derivations a binary cache client would actually fetch.

mod filter;
mod missing;

use std::io::Write;
use std::os::fd::AsFd;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use harmonia_store_aterm::parse_derivation_aterm;
use harmonia_store_derivation::derivation::Derivation;
use harmonia_store_derivation::derived_path::DerivedPath;
use harmonia_store_path::{StoreDir, StorePath};
use serde_json::Value;

use crate::filter::SkipRule;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Also skip `preferLocalBuild` derivations, even though clients could fetch them
    #[arg(long)]
    skip_prefer_local: bool,

    /// Run this `sh` command with the built output paths on stdin
    #[arg(long, value_name = "CMD")]
    push_command: Option<String>,

    /// What to build, as for `nix build` (for example `.#package`)
    #[arg(required = true)]
    installables: Vec<String>,

    /// Extra `nix build` flags, such as `-L` or `--impure`.
    /// `--expr` and `--file` are not supported
    #[arg(last = true, value_name = "NIX_ARGS")]
    nix_args: Vec<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let settings = setting_args(&cli.nix_args);

    let targets = evaluate(&cli)?;
    let store_dir = store_dir_of(&targets)?;
    let derived_paths = targets
        .iter()
        .map(|target| store_dir.parse(target))
        .collect::<Result<Vec<DerivedPath>, _>>()
        .context("parsing the paths `nix build` reported")?;

    let missing = missing::query_missing(&store_dir, &derived_paths, &settings).await?;
    for path in &missing.unknown {
        eprintln!("warning: don't know how to build {path}");
    }

    let rule = SkipRule {
        always_allow_substitutes: read_bool_setting("always-allow-substitutes", &settings)?,
        skip_prefer_local: cli.skip_prefer_local,
    };
    let mut to_build = Vec::new();
    for drv_path in &missing.will_build {
        if !rule.skips(&read_derivation(&store_dir, drv_path)?) {
            to_build.push(format!("{}^*", store_dir.display(drv_path)));
        }
    }
    eprintln!(
        "{} to build, {} skipped, {} already cached",
        to_build.len(),
        missing.will_build.len() - to_build.len(),
        missing.will_substitute.len(),
    );
    if to_build.is_empty() {
        return Ok(());
    }

    let out_paths = build(&to_build, &cli.nix_args)?;
    print!("{out_paths}");
    std::io::stdout().flush()?;
    if let Some(command) = &cli.push_command {
        push(command, &out_paths)?;
    }
    Ok(())
}

/// Evaluates the installables and returns what they resolve to, in `DerivedPath` syntax.
///
/// Going through `nix build` keeps flake refs, fragments and `^out` handling identical to Nix.
fn evaluate(cli: &Cli) -> Result<Vec<String>> {
    let output = Command::new("nix")
        .args(["build", "--dry-run", "--json", "--no-link"])
        .args(&cli.installables)
        .args(&cli.nix_args)
        .stderr(Stdio::inherit())
        .output()
        .context("running `nix build --dry-run`")?;
    ensure!(output.status.success(), "evaluation failed");
    parse_targets(&serde_json::from_slice(&output.stdout)?)
}

/// Converts the JSON of `nix build --dry-run --json` into `DerivedPath` syntax.
fn parse_targets(json: &Value) -> Result<Vec<String>> {
    let unexpected = || format!("unexpected `nix build --json` output: {json}");
    let entries = json.as_array().with_context(unexpected)?;
    entries
        .iter()
        .map(|entry| {
            if let Some(path) = entry["path"].as_str() {
                return Ok(path.to_owned());
            }
            let drv_path = entry["drvPath"].as_str().with_context(unexpected)?;
            let outputs = entry["outputs"].as_object().with_context(unexpected)?;
            let names: Vec<&str> = outputs.keys().map(String::as_str).collect();
            Ok(format!("{drv_path}^{}", names.join(",")))
        })
        .collect()
}

fn store_dir_of(targets: &[String]) -> Result<StoreDir> {
    let first = targets.first().context("nothing to build")?;
    let parent = Path::new(first)
        .parent()
        .context("store path has no parent")?;
    let parent = parent.to_str().context("store dir is not UTF-8")?;
    parent.parse().context("parsing the store dir")
}

/// Returns the settings in `nix_args` that every `nix` subcommand accepts.
///
/// `nix daemon` and `nix config show` reject evaluation flags such as `--impure`.
/// shortcut: other forms such as `--substituters x` are not forwarded, parse them if needed.
fn setting_args(nix_args: &[String]) -> Vec<String> {
    let mut settings = Vec::new();
    let mut args = nix_args.iter();
    while let Some(arg) = args.next() {
        if arg == "--option" {
            settings.push(arg.clone());
            settings.extend(args.by_ref().take(2).cloned());
        }
    }
    settings
}

fn read_bool_setting(name: &str, settings: &[String]) -> Result<bool> {
    let output = Command::new("nix")
        .args(["config", "show", name])
        .args(settings)
        .stderr(Stdio::inherit())
        .output()
        .context("running `nix config show`")?;
    ensure!(output.status.success(), "`nix config show {name}` failed");
    match String::from_utf8_lossy(&output.stdout).trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => bail!("setting {name} has unexpected value {other:?}"),
    }
}

/// shortcut: reads the store directly, so chroot and remote stores are unsupported. Fetch the
/// `.drv` over the daemon protocol if those are ever needed.
fn read_derivation(store_dir: &StoreDir, drv_path: &StorePath) -> Result<Derivation> {
    let file = store_dir.to_path().join(drv_path.to_string());
    let aterm = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
    let name = drv_path.name().to_string();
    let name = name.strip_suffix(".drv").unwrap_or(&name);
    parse_derivation_aterm(store_dir, &aterm, name.parse()?)
        .with_context(|| format!("parsing {}", file.display()))
}

/// Builds through stdin because large closures would exceed the argv limit.
fn build(to_build: &[String], nix_args: &[String]) -> Result<String> {
    let mut child = Command::new("nix")
        .args(["build", "--stdin", "--no-link", "--print-out-paths"])
        .args(nix_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("running `nix build`")?;
    let mut stdin = child.stdin.take().context("nix build stdin")?;
    stdin.write_all(to_build.join("\n").as_bytes())?;
    drop(stdin);

    let output = child.wait_with_output()?;
    ensure!(output.status.success(), "`nix build` failed");
    Ok(String::from_utf8(output.stdout)?)
}

fn push(command: &str, out_paths: &str) -> Result<()> {
    let mut child = Command::new("sh")
        .args(["-c", command])
        .stdin(Stdio::piped())
        // Our stdout is reserved for the output paths.
        .stdout(std::io::stderr().as_fd().try_clone_to_owned()?)
        .spawn()
        .context("spawning the push command")?;
    let mut stdin = child.stdin.take().context("push command stdin")?;
    stdin.write_all(out_paths.as_bytes())?;
    drop(stdin);
    ensure!(child.wait()?.success(), "the push command failed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn targets_keep_requested_outputs_and_plain_paths() {
        let json = json!([
            {"drvPath": "/s/a.drv", "outputs": {"man": "/s/a-man", "out": "/s/a"}},
            {"path": "/s/b"},
        ]);
        assert_eq!(parse_targets(&json).unwrap(), ["/s/a.drv^man,out", "/s/b"]);
    }

    #[test]
    fn unexpected_json_is_an_error() {
        assert!(parse_targets(&json!({"drvPath": "/s/a.drv"})).is_err());
        assert!(parse_targets(&json!([{"drvPath": "/s/a.drv"}])).is_err());
    }

    #[test]
    fn only_option_flags_reach_every_subcommand() {
        let args = ["-L", "--option", "cores", "4", "--impure"].map(String::from);
        assert_eq!(setting_args(&args), ["--option", "cores", "4"]);
    }
}
