//! Tells the user what is actually going to happen.

use harmonia_protocol::daemon_wire::types2::QueryMissingResult;
use harmonia_store_path::{StoreDir, StorePath};

/// Recognises the list of missing paths that `nix build --dry-run` always logs.
///
/// That list includes what gets skipped, so it would contradict `describe_plan`.
/// shortcut: matches Nix's human-readable text. If that changes, the list merely reappears.
#[derive(Default)]
pub struct MissingListFilter {
    in_list: bool,
}

impl MissingListFilter {
    /// Returns whether this line of Nix's log should be shown.
    pub fn keeps(&mut self, line: &str) -> bool {
        if self.in_list && line.starts_with("  ") {
            return false;
        }
        self.in_list = is_missing_list_header(line);
        !self.in_list
    }
}

fn is_missing_list_header(line: &str) -> bool {
    let counted = line.starts_with("this ") || line.starts_with("these ");
    (counted && (line.ends_with(" will be built:") || line.contains(" will be fetched (")))
        || line.starts_with("don't know how to build these paths")
}

/// Describes the outcome, given Nix's answers for everything and for just what gets requested.
pub fn describe_plan(
    store_dir: &StoreDir,
    requested: &[&StorePath],
    everything: &QueryMissingResult,
    planned: &QueryMissingResult,
) -> String {
    let skipped: Vec<&StorePath> = everything
        .will_build
        .difference(&planned.will_build)
        .collect();
    let to_build: Vec<&StorePath> = planned.will_build.iter().collect();
    let to_fetch: Vec<&StorePath> = planned.will_substitute.iter().collect();

    let mut plan = section(store_dir, "requested", "derivation", "", requested);
    plan += &section(store_dir, "will build", "derivation", "", &to_build);
    plan += &section(store_dir, "skipped", "derivation", "", &skipped);
    let sizes = format!(
        " ({} download, {} unpacked)",
        mebibytes(planned.download_size),
        mebibytes(planned.nar_size)
    );
    plan += &section(store_dir, "will fetch", "path", &sizes, &to_fetch);
    plan + &format!(
        "{} requested, {} to build including dependencies, {} skipped, {} to fetch",
        requested.len(),
        to_build.len(),
        skipped.len(),
        to_fetch.len(),
    )
}

fn section(
    store_dir: &StoreDir,
    action: &str,
    noun: &str,
    details: &str,
    paths: &[&StorePath],
) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let plural = if paths.len() == 1 { "" } else { "s" };
    let mut sorted = paths.to_vec();
    sorted.sort_by_key(|path| path.name().to_string());

    let mut text = format!("{action} {} {noun}{plural}{details}:\n", paths.len());
    for path in sorted {
        text += &format!("  {}\n", store_dir.display(path));
    }
    text
}

fn mebibytes(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn kept(log: &[&str]) -> Vec<String> {
        let mut filter = MissingListFilter::default();
        log.iter()
            .filter(|line| filter.keeps(line))
            .map(ToString::to_string)
            .collect()
    }

    fn paths(names: &[&str]) -> BTreeSet<StorePath> {
        names
            .iter()
            .map(|name| {
                StoreDir::default()
                    .parse(&format!("/nix/store/{name}"))
                    .unwrap()
            })
            .collect()
    }

    fn query_result(will_build: &[&str], will_substitute: &[&str]) -> QueryMissingResult {
        QueryMissingResult {
            will_build: paths(will_build),
            will_substitute: paths(will_substitute),
            unknown: BTreeSet::new(),
            download_size: 1024 * 1024,
            nar_size: 3 * 1024 * 1024 / 2,
        }
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-z.drv";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-a.drv";
    const C: &str = "cccccccccccccccccccccccccccccccc-skip.drv";
    const F: &str = "ffffffffffffffffffffffffffffffff-input";

    #[test]
    fn every_list_nix_prints_is_dropped() {
        let log = [
            "warning: before",
            "these 2 derivations will be built:",
            "  /nix/store/a.drv",
            "  /nix/store/b.drv",
            "this derivation will be built:",
            "  /nix/store/c.drv",
            "these 3 paths will be fetched (1.0 MiB download, 2.0 MiB unpacked):",
            "  /nix/store/d",
            "this path will be fetched (1.0 KiB download, 2.0 KiB unpacked):",
            "  /nix/store/e",
            "don't know how to build these paths:",
            "  /nix/store/f",
            "warning: after",
        ];
        assert_eq!(kept(&log), ["warning: before", "warning: after"]);
    }

    #[test]
    fn indented_lines_outside_a_list_are_kept() {
        let log = [
            "error:",
            "  … while evaluating these 2 derivations will be built:",
            "  at flake.nix:1:1",
        ];
        assert_eq!(kept(&log), log);
    }

    #[test]
    fn plan_separates_built_skipped_and_fetched_paths() {
        let requested = paths(&[A]);
        let plan = describe_plan(
            &StoreDir::default(),
            &requested.iter().collect::<Vec<_>>(),
            &query_result(&[A, B, C], &[]),
            &query_result(&[A, B], &[F]),
        );
        let expected = format!(
            "requested 1 derivation:\n  /nix/store/{A}\n\
             will build 2 derivations:\n  /nix/store/{B}\n  /nix/store/{A}\n\
             skipped 1 derivation:\n  /nix/store/{C}\n\
             will fetch 1 path (1.0 MiB download, 1.5 MiB unpacked):\n  /nix/store/{F}\n\
             1 requested, 2 to build including dependencies, 1 skipped, 1 to fetch"
        );
        assert_eq!(plan, expected);
    }

    #[test]
    fn empty_sections_are_left_out() {
        let empty = query_result(&[], &[]);
        let plan = describe_plan(&StoreDir::default(), &[], &empty, &empty);
        assert_eq!(
            plan,
            "0 requested, 0 to build including dependencies, 0 skipped, 0 to fetch"
        );
    }
}
