//! Builds only the derivations a binary cache client would actually fetch.

mod daemon;
mod filter;
mod report;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsFd;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use harmonia_store_aterm::parse_derivation_aterm;
use harmonia_store_derivation::derivation::Derivation;
use harmonia_store_derivation::derived_path::{
    DerivedPath, OutputName, OutputSpec, SingleDerivedPath,
};
use harmonia_store_path::{StoreDir, StorePath};
use serde_json::Value;

use crate::daemon::NixDaemon;
use crate::filter::SkipRule;
use crate::report::{MissingListFilter, describe_plan};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Also skip `preferLocalBuild` derivations, even though clients could fetch them
    #[arg(long)]
    skip_prefer_local: bool,

    /// Also skip what looks like a `writeShellApplication` script. This is a best-effort match:
    /// clients could still fetch these, and they will build them locally instead
    #[arg(long)]
    skip_shell_applications: bool,

    /// Show what would be built, skipped and fetched, without building or pushing anything
    #[arg(long)]
    dry_run: bool,

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
    let derived_paths = parse_derived_paths(&store_dir, &targets)?;

    let mut daemon = NixDaemon::connect(&store_dir, &settings).await?;
    let missing = daemon.query_missing(&derived_paths).await?;
    for path in &missing.unknown {
        eprintln!("warning: don't know how to build {path}");
    }

    let rule = SkipRule {
        always_allow_substitutes: read_bool_setting("always-allow-substitutes", &settings)?,
        skip_prefer_local: cli.skip_prefer_local,
        skip_shell_applications: cli.skip_shell_applications,
    };
    let requested = find_requests(
        &mut daemon,
        &store_dir,
        rule,
        &missing.will_build,
        &derived_paths,
    )
    .await?;
    let to_build: Vec<String> = requested
        .iter()
        .map(|(drv_path, outputs)| {
            let names: Vec<String> = outputs.iter().map(ToString::to_string).collect();
            format!("{}^{}", store_dir.display(drv_path), names.join(","))
        })
        .collect();

    // Asked again because skipped derivations can still be needed by the requested ones, and what
    // they alone needed is no longer fetched.
    let planned = daemon
        .query_missing(&parse_derived_paths(&store_dir, &to_build)?)
        .await?;
    drop(daemon);
    let requested_drvs: Vec<&StorePath> = requested.keys().collect();
    eprintln!(
        "{}",
        describe_plan(&store_dir, &requested_drvs, &missing, &planned)
    );
    if cli.dry_run || to_build.is_empty() {
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

/// Returns what to ask `nix build` for: the outputs a client fetches, by derivation.
///
/// A client fetches each target, so its dependencies are left for Nix to build as needed. Only for
/// a skipped derivation, which the client builds itself, does it fetch the inputs instead.
async fn find_requests(
    daemon: &mut NixDaemon,
    store_dir: &StoreDir,
    rule: SkipRule,
    will_build: &BTreeSet<StorePath>,
    targets: &[DerivedPath],
) -> Result<BTreeMap<StorePath, BTreeSet<OutputName>>> {
    let mut pending = Vec::new();
    for target in targets {
        // `parse_targets` always names the outputs, and plain store paths are never built.
        if let DerivedPath::Built {
            drv_path,
            outputs: OutputSpec::Named(names),
        } = target
        {
            let drv_path = drv_path.root_path();
            pending.extend(names.iter().map(|name| (drv_path.clone(), name.clone())));
        }
    }

    let mut requested = BTreeMap::<StorePath, BTreeSet<OutputName>>::new();
    let mut skipped = BTreeSet::new();
    while let Some((drv_path, output)) = pending.pop() {
        if !will_build.contains(&drv_path) || skipped.contains(&drv_path) {
            continue;
        }
        if let Some(outputs) = requested.get_mut(&drv_path) {
            outputs.insert(output);
            continue;
        }
        let drv = read_derivation(daemon, store_dir, &drv_path).await?;
        if rule.skips(&drv) {
            pending.extend(drv.inputs.iter().filter_map(input_drv_output));
            skipped.insert(drv_path);
        } else {
            requested.insert(drv_path, BTreeSet::from([output]));
        }
    }

    Ok(requested)
}

/// Returns the derivation output an input needs first, or nothing for a plain source.
///
/// For a dynamic derivation's input, that is the output that produces the inner `.drv`.
fn input_drv_output(input: &SingleDerivedPath) -> Option<(StorePath, OutputName)> {
    let SingleDerivedPath::Built { drv_path, output } = input else {
        return None;
    };
    match drv_path.as_ref() {
        SingleDerivedPath::Opaque(path) => Some((path.clone(), output.clone())),
        nested => input_drv_output(nested),
    }
}

fn parse_derived_paths(store_dir: &StoreDir, paths: &[String]) -> Result<Vec<DerivedPath>> {
    paths
        .iter()
        .map(|path| store_dir.parse(path))
        .collect::<Result<_, _>>()
        .context("parsing derived paths")
}

/// Evaluates the installables and returns what they resolve to, in `DerivedPath` syntax.
///
/// Going through `nix build` keeps flake refs, fragments and `^out` handling identical to Nix.
fn evaluate(cli: &Cli) -> Result<Vec<String>> {
    let mut child = Command::new("nix")
        .args(["build", "--dry-run", "--json", "--no-link"])
        .args(&cli.installables)
        .args(&cli.nix_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running `nix build --dry-run`")?;
    let log = child.stderr.take().context("nix build stderr")?;
    let forwarder = std::thread::spawn(move || forward_evaluation_log(log));

    let output = child.wait_with_output()?;
    forwarder
        .join()
        .expect("the log forwarder panicked")
        .context("reading the `nix build --dry-run` log")?;
    ensure!(output.status.success(), "evaluation failed");
    parse_targets(&serde_json::from_slice(&output.stdout)?)
}

/// Shows warnings and errors as they happen, without Nix's unfiltered list of missing paths.
fn forward_evaluation_log(log: impl Read) -> std::io::Result<()> {
    let mut filter = MissingListFilter::default();
    for line in BufReader::new(log).split(b'\n') {
        let line = line?;
        if filter.keeps(&String::from_utf8_lossy(&line)) {
            let mut stderr = std::io::stderr().lock();
            stderr.write_all(&line)?;
            stderr.write_all(b"\n")?;
        }
    }
    Ok(())
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
/// `nix daemon` and `nix config show` reject evaluation flags such as `--impure`. A store chosen
/// through `nix.conf` or `NIX_REMOTE` needs no forwarding, as child processes read those too.
/// shortcut: other forms such as `--substituters x` are not forwarded, parse them if needed.
fn setting_args(nix_args: &[String]) -> Vec<String> {
    let mut settings = Vec::new();
    let mut args = nix_args.iter();
    while let Some(arg) = args.next() {
        let value_count = match arg.as_str() {
            "--option" => 2,
            "--store" => 1,
            _ => continue,
        };
        settings.push(arg.clone());
        settings.extend(args.by_ref().take(value_count).cloned());
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

async fn read_derivation(
    daemon: &mut NixDaemon,
    store_dir: &StoreDir,
    drv_path: &StorePath,
) -> Result<Derivation> {
    let aterm = daemon.read_file(drv_path).await?;
    let name = drv_path.name().to_string();
    let name = name.strip_suffix(".drv").unwrap_or(&name);
    parse_derivation_aterm(store_dir, &aterm, name.parse()?)
        .with_context(|| format!("parsing {drv_path}"))
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
    fn dynamic_input_needs_the_output_that_produces_its_derivation() {
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a.drv";
        let store_dir = StoreDir::default();
        let input: SingleDerivedPath = store_dir.parse(&format!("{drv}^inner.drv^out")).unwrap();

        let (path, output) = input_drv_output(&input).unwrap();

        assert_eq!(store_dir.display(&path).to_string(), drv);
        assert_eq!(output.to_string(), "inner.drv");
    }

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
    fn only_settings_reach_every_subcommand() {
        let args = ["-L", "--option", "cores", "4", "--impure", "--store", "/s"].map(String::from);
        assert_eq!(
            setting_args(&args),
            ["--option", "cores", "4", "--store", "/s"]
        );
    }

    #[test]
    fn flag_values_are_not_mistaken_for_flags() {
        let args = ["--option", "--store", "x", "--impure"].map(String::from);
        assert_eq!(setting_args(&args), ["--option", "--store", "x"]);
    }
}
