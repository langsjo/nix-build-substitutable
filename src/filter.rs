//! Decides which derivations are worth building for a binary cache.

use harmonia_store_derivation::derivation::Derivation;

#[derive(Clone, Copy)]
pub struct SkipRule {
    /// Value of the Nix setting `always-allow-substitutes`.
    pub always_allow_substitutes: bool,
    /// Also skip `preferLocalBuild` derivations, which clients could still fetch.
    pub skip_prefer_local: bool,
    /// Also skip what looks like a `writeShellApplication` script.
    pub skip_shell_applications: bool,
}

impl SkipRule {
    pub fn skips(self, drv: &Derivation) -> bool {
        let client_cannot_fetch =
            !self.always_allow_substitutes && !bool_attr(drv, "allowSubstitutes", true);
        client_cannot_fetch
            || (self.skip_prefer_local && bool_attr(drv, "preferLocalBuild", false))
            || (self.skip_shell_applications && looks_like_shell_application(drv))
    }
}

/// Recognises `writeShellApplication`, as derivations don't record which builder made them.
///
/// shortcut: relies on how nixpkgs writes these today. If that changes, they just get built again.
fn looks_like_shell_application(drv: &Derivation) -> bool {
    // Unusual for a text file: nixpkgs sets these explicitly so clients can fetch the script.
    let explicitly_fetchable =
        bool_attr(drv, "allowSubstitutes", false) && !bool_attr(drv, "preferLocalBuild", true);
    let build_command = string_attr(drv, "buildCommand");
    // Older nixpkgs has no `destination`, and hardcodes the path in `buildCommand` instead.
    let in_bin = string_attr(drv, "destination").starts_with(b"/bin/")
        || contains(build_command, b"target=$out/bin/");

    explicitly_fetchable
        && bool_attr(drv, "executable", false)
        && contains(build_command, b"target=$out")
        && in_bin
        && has_bash_shebang(string_attr(drv, "text"))
}

fn has_bash_shebang(text: &[u8]) -> bool {
    let first_line = text.split(|byte| *byte == b'\n').next().unwrap_or_default();
    first_line.starts_with(b"#!")
        && first_line.ends_with(b"/bin/bash")
        && !first_line.iter().any(u8::is_ascii_whitespace)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|part| part == needle)
}

/// Returns an empty string for attributes that are missing or not strings.
fn string_attr<'a>(drv: &'a Derivation, name: &str) -> &'a [u8] {
    let value = match &drv.structured_attrs {
        Some(structured) => structured
            .attrs
            .get(name)
            .and_then(|value| value.as_str())
            .map(str::as_bytes),
        None => drv.env.get(name.as_bytes()).map(|value| value.as_ref()),
    };
    value.unwrap_or_default()
}

/// Mirrors `getBoolAttr` in Nix's `derivation-options.cc`.
fn bool_attr(drv: &Derivation, name: &str, default: bool) -> bool {
    match &drv.structured_attrs {
        // Nix's own queryMissing has already rejected non-boolean values.
        Some(structured) => structured
            .attrs
            .get(name)
            .and_then(|value| value.as_bool())
            .unwrap_or(default),
        None => drv
            .env
            .get(name.as_bytes())
            .map_or(default, |value| value.as_ref() == b"1"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harmonia_store_aterm::parse_derivation_aterm;
    use harmonia_store_path::StoreDir;

    const DEFAULT: SkipRule = SkipRule {
        always_allow_substitutes: false,
        skip_prefer_local: false,
        skip_shell_applications: false,
    };
    const SHELL_APPLICATIONS: SkipRule = SkipRule {
        skip_shell_applications: true,
        ..DEFAULT
    };

    /// Parses real ATerm so that the tests also pin how `__json` reaches `structured_attrs`.
    fn drv(env: &str) -> Derivation {
        let aterm = format!(
            r#"Derive([("out","/nix/store/ffffffffffffffffffffffffffffffff-t","","")],[],[],"x86_64-linux","/bin/sh",[],[{env}])"#
        );
        parse_derivation_aterm(&StoreDir::default(), aterm.as_bytes(), "t".parse().unwrap())
            .unwrap()
    }

    #[test]
    fn env_attribute_is_true_only_when_exactly_one() {
        assert!(!DEFAULT.skips(&drv("")), "unset defaults to substitutable");
        assert!(!DEFAULT.skips(&drv(r#"("allowSubstitutes","1")"#)));
        assert!(DEFAULT.skips(&drv(r#"("allowSubstitutes","")"#)));
        assert!(DEFAULT.skips(&drv(r#"("allowSubstitutes","0")"#)));
    }

    #[test]
    fn structured_attrs_are_read_from_json() {
        let disallowed = drv(r#"("__json","{\"allowSubstitutes\":false}")"#);
        let allowed = drv(r#"("__json","{\"allowSubstitutes\":true}")"#);
        assert!(DEFAULT.skips(&disallowed));
        assert!(!DEFAULT.skips(&allowed));
    }

    #[test]
    fn env_is_ignored_when_structured_attrs_are_present() {
        // Nix only looks at the JSON in this case, and so must we.
        let drv = drv(r#"("__json","{}"),("allowSubstitutes","")"#);
        assert!(!DEFAULT.skips(&drv));
    }

    #[test]
    fn prefer_local_is_only_skipped_on_request() {
        let drv = drv(r#"("preferLocalBuild","1")"#);
        assert!(!DEFAULT.skips(&drv));
        let rule = SkipRule {
            skip_prefer_local: true,
            ..DEFAULT
        };
        assert!(rule.skips(&drv));
    }

    #[test]
    fn always_allow_substitutes_makes_everything_fetchable() {
        let rule = SkipRule {
            always_allow_substitutes: true,
            ..DEFAULT
        };
        assert!(!rule.skips(&drv(r#"("allowSubstitutes","")"#)));
    }

    /// Real `.drv` files named `<nixpkgs version>--<variant>--<store path name>.drv`.
    fn fixtures(kind: &str) -> Vec<(String, Derivation)> {
        let directory = format!(
            "{}/testdata/shell-applications/{kind}",
            env!("CARGO_MANIFEST_DIR")
        );
        let mut fixtures = Vec::new();
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let file_name = entry.file_name().into_string().unwrap();
            let store_name = file_name.rsplit("--").next().unwrap();
            let name = store_name.split_once('-').unwrap().1;
            let name = name.strip_suffix(".drv").unwrap().parse().unwrap();
            let aterm = std::fs::read(entry.path()).unwrap();
            let drv = parse_derivation_aterm(&StoreDir::default(), &aterm, name).unwrap();
            fixtures.push((file_name, drv));
        }
        fixtures
    }

    #[test]
    fn real_shell_applications_are_recognised() {
        let fixtures = fixtures("real");
        assert_eq!(fixtures.len(), 16);
        for (file_name, drv) in fixtures {
            assert!(SHELL_APPLICATIONS.skips(&drv), "{file_name}");
            assert!(!DEFAULT.skips(&drv), "{file_name} needs the flag");
        }
    }

    #[test]
    fn real_lookalikes_are_not_recognised() {
        let fixtures = fixtures("lookalike");
        assert_eq!(fixtures.len(), 21);
        for (file_name, drv) in fixtures {
            assert!(!looks_like_shell_application(&drv), "{file_name}");
        }
    }

    #[test]
    fn every_part_of_the_shape_is_required() {
        const SHAPE: [(&str, &str); 5] = [
            ("allowSubstitutes", "1"),
            ("preferLocalBuild", ""),
            ("executable", "1"),
            ("buildCommand", "target=$out/bin/app"),
            ("text", r"#!/nix/store/x/bin/bash\nset -o errexit\n"),
        ];
        let env = |attrs: Vec<(&str, &str)>| {
            let attrs: Vec<String> = attrs
                .iter()
                .map(|(name, value)| format!(r#"("{name}","{value}")"#))
                .collect();
            drv(&attrs.join(","))
        };
        assert!(looks_like_shell_application(&env(SHAPE.to_vec())));

        for missing in SHAPE {
            let attrs = SHAPE.into_iter().filter(|attr| *attr != missing).collect();
            assert!(!looks_like_shell_application(&env(attrs)), "{missing:?}");
        }
    }

    #[test]
    fn scripts_outside_bin_are_not_recognised() {
        let drv = drv(concat!(
            r#"("allowSubstitutes","1"),("preferLocalBuild",""),("executable","1"),"#,
            r##"("buildCommand","target=$out/etc/app"),("text","#!/nix/store/x/bin/bash\n")"##,
        ));
        assert!(!looks_like_shell_application(&drv));
    }

    #[test]
    fn other_builders_using_destination_are_not_recognised() {
        let drv = drv(concat!(
            r#"("allowSubstitutes","1"),("preferLocalBuild",""),("executable","1"),"#,
            r##"("destination","/bin/app"),("text","#!/nix/store/x/bin/bash\n")"##,
        ));
        assert!(!looks_like_shell_application(&drv));
    }

    #[test]
    fn only_a_bash_shebang_counts() {
        assert!(has_bash_shebang(b"#!/nix/store/x/bin/bash\necho"));
        assert!(!has_bash_shebang(b"#!/nix/store/x/bin/sh\necho"));
        assert!(!has_bash_shebang(b"#!/usr/bin/env python /bin/bash\necho"));
        assert!(!has_bash_shebang(b"echo /bin/bash\n"));
    }
}
