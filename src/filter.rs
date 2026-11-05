//! Decides which derivations are worth building for a binary cache.

use harmonia_store_derivation::derivation::Derivation;

#[derive(Clone, Copy)]
pub struct SkipRule {
    /// Value of the Nix setting `always-allow-substitutes`.
    pub always_allow_substitutes: bool,
    /// Also skip `preferLocalBuild` derivations, which clients could still fetch.
    pub skip_prefer_local: bool,
}

impl SkipRule {
    pub fn skips(self, drv: &Derivation) -> bool {
        let client_cannot_fetch =
            !self.always_allow_substitutes && !bool_attr(drv, "allowSubstitutes", true);
        client_cannot_fetch || (self.skip_prefer_local && bool_attr(drv, "preferLocalBuild", false))
    }
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
}
