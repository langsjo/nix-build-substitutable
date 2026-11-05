//! Runs the binary against real Nix, in a private store that leaves the host store untouched.
//!
//! Every test gets its own store, a `file://` binary cache, and optionally its own daemon.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const FLAKE: &str = r##"{
  outputs = { self }: let
    mk = name: script: attrs: derivation ({
      inherit name;
      system = "@system@";
      builder = "/bin/sh";
      args = [ "-c" script ];
    } // attrs);
    file = name: attrs: mk name "echo ${name} > $out" attrs;
    plain = file "plain" { };
    cached = file "cached" { };
    preferLocal = file "prefer-local" { preferLocalBuild = true; };
    localOnly = { allowSubstitutes = false; preferLocalBuild = true; };
  in {
    packages."@system@" = {
      inherit plain cached;
      inherit preferLocal;
      traced = builtins.trace "trace-from-evaluation" (file "traced" { });
      usesPreferLocal = mk "uses-prefer-local" "echo ${preferLocal} > $out" { };
      usesCached = mk "uses-cached" "echo ${cached} > $out" { };
      structuredLocal = file "structured-local" { __structuredAttrs = true; allowSubstitutes = false; };
      fails = mk "fails" "exit 1" { };

      # Shaped like a NixOS toplevel: a client always builds it locally from its inputs.
      toplevel = mk "toplevel" "echo ${plain} ${cached} > $out" localOnly;
      cachedInputOnly = mk "cached-input-only" "echo ${cached} > $out" localOnly;
    };
  };
}"##;

#[derive(Clone, Copy, PartialEq)]
enum Store {
    /// A single-user install: no daemon.
    Local,
    /// A multi-user install: settings must travel from the client to the daemon.
    Daemon,
}

struct Sandbox {
    root: TempDir,
    flake: String,
    remote: &'static str,
    daemon: Option<Child>,
}

impl Sandbox {
    fn new(store: Store) -> Self {
        let root = TempDir::new().unwrap();
        let conf_dir = root.path().join("etc");
        fs::create_dir_all(&conf_dir).unwrap();
        fs::write(
            conf_dir.join("nix.conf"),
            "experimental-features = nix-command flakes\n\
             sandbox = false\nbuild-users-group =\nrequire-sigs = false\n\
             trusted-users = *\nsubstituters =\nflake-registry =\n\
             narinfo-cache-negative-ttl = 0\n",
        )
        .unwrap();

        let flake_dir = root.path().join("flake");
        fs::create_dir(&flake_dir).unwrap();
        let system = String::from_utf8(
            Command::new("nix")
                .args([
                    "eval",
                    "--raw",
                    "--impure",
                    "--expr",
                    "builtins.currentSystem",
                ])
                .args(["--extra-experimental-features", "nix-command"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        fs::write(
            flake_dir.join("flake.nix"),
            FLAKE.replace("@system@", &system),
        )
        .unwrap();

        let mut sandbox = Self {
            flake: format!("path:{}", flake_dir.display()),
            root,
            remote: "local",
            daemon: None,
        };
        if store == Store::Daemon {
            sandbox.start_daemon();
        }
        sandbox
    }

    fn start_daemon(&mut self) {
        let socket = self.socket();
        let mut daemon = Command::new("nix");
        self.apply_env(&mut daemon);
        // The daemon gets its own user config, so it only sees what clients forward to it.
        let daemon = daemon
            .env("NIX_REMOTE", "local")
            .env("XDG_CONFIG_HOME", self.root.path().join("daemon-config"))
            .env("XDG_CACHE_HOME", self.root.path().join("daemon-cache"))
            .arg("daemon")
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.daemon = Some(daemon);
        self.remote = "daemon";

        let deadline = Instant::now() + Duration::from_secs(30);
        while !socket.exists() {
            assert!(Instant::now() < deadline, "daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn socket(&self) -> PathBuf {
        self.root.path().join("daemon-socket")
    }

    fn cache_url(&self) -> String {
        format!("file://{}", self.root.path().join("cache").display())
    }

    fn apply_env(&self, command: &mut Command) {
        let root = self.root.path();
        command
            .env("NIX_STORE_DIR", root.join("store"))
            .env("NIX_STATE_DIR", root.join("state"))
            .env("NIX_LOG_DIR", root.join("log"))
            .env("NIX_CONF_DIR", root.join("etc"))
            .env("NIX_DAEMON_SOCKET_PATH", self.socket())
            .env("NIX_REMOTE", self.remote)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache-home"))
            .env("HOME", root.join("home"))
            .env_remove("NIX_CONFIG");
    }

    fn nix(&self, args: &[&str]) -> Output {
        let mut command = Command::new("nix");
        self.apply_env(&mut command);
        command.args(args).output().unwrap()
    }

    fn nix_ok(&self, args: &[&str]) -> String {
        let output = self.nix(args);
        assert!(
            output.status.success(),
            "nix {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn installable(&self, package: &str) -> String {
        format!("{}#{package}", self.flake)
    }

    fn out_path(&self, package: &str) -> String {
        self.nix_ok(&[
            "eval",
            "--raw",
            &format!("{}.outPath", self.installable(package)),
        ])
    }

    fn is_valid(&self, package: &str) -> bool {
        let path = self.out_path(package);
        self.nix(&["path-info", &path]).status.success()
    }

    /// Puts `cached` in the binary cache and removes it from the store.
    fn populate_cache(&self) {
        let cached = self.installable("cached");
        let path = self.nix_ok(&["build", "--no-link", "--print-out-paths", &cached]);
        self.nix_ok(&["copy", "--to", &self.cache_url(), &path]);
        self.nix_ok(&["store", "delete", &path]);
        assert!(!self.is_valid("cached"), "fixture must start out uncached");
    }

    /// Runs nix-build-substitutable with the binary cache configured as a substituter.
    fn run(&self, flags: &[&str], packages: &[&str], extra_nix_args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nix-build-substitutable"));
        self.apply_env(&mut command);
        command.args(flags);
        command.args(packages.iter().map(|package| self.installable(package)));
        command.args(["--", "--option", "substituters", &self.cache_url()]);
        command.args(extra_nix_args);
        command.output().unwrap()
    }

    fn run_ok(&self, flags: &[&str], packages: &[&str]) -> Vec<String> {
        let output = self.run(flags, packages, &[]);
        assert!(
            output.status.success(),
            "nix-build-substitutable failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        lines(&output.stdout)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(daemon) = &mut self.daemon {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        // Store paths are read-only, which would stop TempDir from cleaning them up.
        let _ = Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(self.root.path())
            .status();
    }
}

fn lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(String::from)
        .collect()
}

fn check_toplevel_is_skipped_but_its_uncached_input_is_built(store: Store) {
    let sandbox = Sandbox::new(store);
    sandbox.populate_cache();

    let printed = sandbox.run_ok(&[], &["toplevel"]);

    assert_eq!(printed, [sandbox.out_path("plain")]);
    assert!(sandbox.is_valid("plain"));
    assert!(!sandbox.is_valid("toplevel"));
    assert!(
        !sandbox.is_valid("cached"),
        "must not download cached inputs"
    );
}

#[test]
fn toplevel_is_skipped_but_its_uncached_input_is_built_without_daemon() {
    check_toplevel_is_skipped_but_its_uncached_input_is_built(Store::Local);
}

#[test]
fn toplevel_is_skipped_but_its_uncached_input_is_built_with_daemon() {
    check_toplevel_is_skipped_but_its_uncached_input_is_built(Store::Daemon);
}

#[test]
fn nothing_is_downloaded_when_all_inputs_are_cached() {
    let sandbox = Sandbox::new(Store::Local);
    sandbox.populate_cache();

    let printed = sandbox.run_ok(&[], &["cachedInputOnly"]);

    assert!(printed.is_empty());
    assert!(!sandbox.is_valid("cached"));
}

#[test]
fn substituter_from_user_config_reaches_the_daemon() {
    let sandbox = Sandbox::new(Store::Daemon);
    sandbox.populate_cache();
    // The way `attic use` configures a cache.
    let config_dir = sandbox.root.path().join("config/nix");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("nix.conf"),
        format!("substituters = {}\n", sandbox.cache_url()),
    )
    .unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_nix-build-substitutable"));
    sandbox.apply_env(&mut command);
    let output = command.arg(sandbox.installable("cached")).output().unwrap();

    assert!(output.status.success());
    assert!(
        lines(&output.stdout).is_empty(),
        "cached must not be rebuilt"
    );
}

#[test]
fn prefer_local_build_is_only_skipped_with_the_flag() {
    let skipping = Sandbox::new(Store::Local);
    assert!(
        skipping
            .run_ok(&["--skip-prefer-local"], &["preferLocal"])
            .is_empty()
    );
    assert!(!skipping.is_valid("preferLocal"));

    let default = Sandbox::new(Store::Local);
    let printed = default.run_ok(&[], &["preferLocal"]);
    assert_eq!(printed, [default.out_path("preferLocal")]);
}

#[test]
fn structured_attrs_can_disallow_substitutes() {
    let sandbox = Sandbox::new(Store::Local);
    assert!(sandbox.run_ok(&[], &["structuredLocal"]).is_empty());
    assert!(!sandbox.is_valid("structuredLocal"));
}

#[test]
fn push_command_receives_the_printed_paths() {
    let sandbox = Sandbox::new(Store::Local);
    let pushed_file = sandbox.root.path().join("pushed");
    let command = format!("cat > {}; echo noise", pushed_file.display());

    let printed = sandbox.run_ok(&["--push-command", &command], &["plain"]);

    assert_eq!(
        printed,
        [sandbox.out_path("plain")],
        "noise must not reach stdout"
    );
    assert_eq!(lines(&fs::read(&pushed_file).unwrap()), printed);
}

#[test]
fn push_command_does_not_run_when_nothing_was_built() {
    let sandbox = Sandbox::new(Store::Local);
    let marker = sandbox.root.path().join("pushed");
    let command = format!("touch {}", marker.display());

    sandbox.run_ok(&["--push-command", &command], &["structuredLocal"]);

    assert!(!Path::new(&marker).exists());
}

#[test]
fn failing_build_exits_nonzero_and_pushes_nothing() {
    let sandbox = Sandbox::new(Store::Local);
    let marker = sandbox.root.path().join("pushed");
    let command = format!("touch {}", marker.display());

    let output = sandbox.run(&["--push-command", &command], &["fails"], &[]);

    assert!(!output.status.success());
    assert!(!marker.exists());
}

#[test]
fn failing_push_command_exits_nonzero() {
    let sandbox = Sandbox::new(Store::Local);
    let output = sandbox.run(
        &["--push-command", "cat >/dev/null; exit 3"],
        &["plain"],
        &[],
    );
    assert!(!output.status.success());
}

#[test]
fn evaluation_flags_only_reach_commands_that_accept_them() {
    let sandbox = Sandbox::new(Store::Local);
    let output = sandbox.run(&[], &["plain"], &["--impure", "-L"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(lines(&output.stdout), [sandbox.out_path("plain")]);
}

#[test]
fn evaluation_error_is_reported() {
    let sandbox = Sandbox::new(Store::Local);
    let output = sandbox.run(&[], &["doesNotExist"], &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("doesNotExist"));
}

/// Every way of choosing a store for one nix-build-substitutable invocation.
enum StoreChoice {
    Flag,
    Option,
    UserConfig,
    NixRemoteEnv,
    NixConfigEnv,
}

/// The default store holds `plain`, and doesn't have the `toplevel` derivation at all. Looking at
/// the default store by mistake would therefore build nothing, or fail to read the derivation.
fn check_all_store_settings_are_honoured(store: Store, choice: StoreChoice) {
    let sandbox = Sandbox::new(store);
    sandbox.nix_ok(&["build", "--no-link", &sandbox.installable("plain")]);
    let other_store = sandbox.root.path().join("other-store");
    let other_store = other_store.to_str().unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_nix-build-substitutable"));
    sandbox.apply_env(&mut command);
    command.arg(sandbox.installable("toplevel"));
    match choice {
        StoreChoice::Flag => {
            command.args(["--", "--store", other_store]);
        }
        StoreChoice::Option => {
            command.args(["--", "--option", "store", other_store]);
        }
        StoreChoice::UserConfig => {
            let config_dir = sandbox.root.path().join("config/nix");
            fs::create_dir_all(&config_dir).unwrap();
            fs::write(
                config_dir.join("nix.conf"),
                format!("store = {other_store}\n"),
            )
            .unwrap();
        }
        StoreChoice::NixRemoteEnv => {
            command.env("NIX_REMOTE", other_store);
        }
        StoreChoice::NixConfigEnv => {
            command.env("NIX_CONFIG", format!("store = {other_store}"));
        }
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "nix-build-substitutable failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut printed = lines(&output.stdout);
    printed.sort();
    let mut expected = [sandbox.out_path("plain"), sandbox.out_path("cached")];
    expected.sort();
    assert_eq!(
        printed, expected,
        "inputs must be built in the chosen store"
    );

    let in_other_store = |path: &str| {
        sandbox
            .nix(&["path-info", "--store", other_store, path])
            .status
            .success()
    };
    assert!(in_other_store(&expected[0]) && in_other_store(&expected[1]));
    assert!(!in_other_store(&sandbox.out_path("toplevel")));
}

#[test]
fn store_flag_is_honoured() {
    check_all_store_settings_are_honoured(Store::Local, StoreChoice::Flag);
}

#[test]
fn store_flag_is_honoured_when_a_daemon_is_the_default_store() {
    check_all_store_settings_are_honoured(Store::Daemon, StoreChoice::Flag);
}

#[test]
fn store_option_is_honoured() {
    check_all_store_settings_are_honoured(Store::Local, StoreChoice::Option);
}

#[test]
fn store_in_user_config_is_honoured() {
    check_all_store_settings_are_honoured(Store::Local, StoreChoice::UserConfig);
}

#[test]
fn nix_remote_env_is_honoured() {
    check_all_store_settings_are_honoured(Store::Local, StoreChoice::NixRemoteEnv);
}

#[test]
fn store_in_nix_config_env_is_honoured() {
    check_all_store_settings_are_honoured(Store::Local, StoreChoice::NixConfigEnv);
}

fn summary_of(output: &Output) -> String {
    assert!(output.status.success(), "nix-build-substitutable failed");
    lines(&output.stderr)
        .into_iter()
        .find(|line| line.contains(" requested, "))
        .expect("no summary line")
}

#[test]
fn summary_leaves_out_what_only_skipped_derivations_needed() {
    let sandbox = Sandbox::new(Store::Local);
    sandbox.populate_cache();
    let summary = summary_of(&sandbox.run(&[], &["toplevel"], &[]));
    // `cached` is an input of the skipped toplevel only, so it isn't fetched.
    assert_eq!(
        summary,
        "1 requested, 1 to build including dependencies, 1 skipped, 0 to fetch"
    );
}

#[test]
fn summary_counts_skipped_derivations_built_as_dependencies() {
    let sandbox = Sandbox::new(Store::Local);
    let output = sandbox.run(&["--skip-prefer-local"], &["usesPreferLocal"], &[]);
    assert_eq!(
        summary_of(&output),
        "1 requested, 2 to build including dependencies, 0 skipped, 0 to fetch"
    );
}

#[test]
fn summary_counts_inputs_to_fetch() {
    let sandbox = Sandbox::new(Store::Local);
    sandbox.populate_cache();
    let summary = summary_of(&sandbox.run(&[], &["usesCached"], &[]));
    assert_eq!(
        summary,
        "1 requested, 1 to build including dependencies, 0 skipped, 1 to fetch"
    );
}

#[test]
fn summary_with_nothing_to_request() {
    let sandbox = Sandbox::new(Store::Local);
    let summary = summary_of(&sandbox.run(&[], &["structuredLocal"], &[]));
    assert_eq!(
        summary,
        "0 requested, 0 to build including dependencies, 1 skipped, 0 to fetch"
    );
}

#[test]
fn only_the_real_plan_is_listed() {
    let sandbox = Sandbox::new(Store::Local);
    sandbox.populate_cache();
    let output = sandbox.run(&[], &["toplevel"], &[]);
    assert!(output.status.success());

    let log = lines(&output.stderr);
    let position = |wanted: &str| log.iter().position(|line| line.starts_with(wanted));
    let plain_drv = |package: &str| {
        sandbox.nix_ok(&[
            "eval",
            "--raw",
            &format!("{}.drvPath", sandbox.installable(package)),
        ])
    };

    let built = position("will build 1 derivation:").expect("no list of builds");
    assert_eq!(log[built + 1], format!("  {}", plain_drv("plain")));
    let skipped = position("skipped 1 derivation:").expect("no list of skipped derivations");
    assert_eq!(log[skipped + 1], format!("  {}", plain_drv("toplevel")));
    assert_eq!(position("will fetch"), None, "nothing should be fetched");
}

#[test]
fn skipped_derivations_are_only_mentioned_as_skipped() {
    let sandbox = Sandbox::new(Store::Local);
    let output = sandbox.run(&[], &["toplevel"], &[]);
    let log = String::from_utf8_lossy(&output.stderr);
    let toplevel_drv = sandbox.nix_ok(&[
        "eval",
        "--raw",
        &format!("{}.drvPath", sandbox.installable("toplevel")),
    ]);
    assert_eq!(log.matches(&toplevel_drv).count(), 1, "{log}");
}

#[test]
fn evaluation_messages_are_still_shown() {
    let sandbox = Sandbox::new(Store::Local);
    let output = sandbox.run(&[], &["traced"], &[]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("trace-from-evaluation"));
}
