//! commitward#22: the README's `[dependencies]` pin must resolve to a crate version that
//! actually carries the anchor (`anchor_checkpoints`, shipped at 0.3.0). The crate's own
//! `version` field is the single source of truth for what "current" means; this asserts the
//! README's pin is compatible with it under cargo's own caret rule, so the two cannot drift
//! apart silently the way `commitward = "0.1"` did after f4d5b1b added the self-protecting
//! anchor to 0.3.0 without the README's pin ever being touched again.
//!
//! No semver crate: cargo's caret rule for a tuple `major.minor[.patch]` pin is "the
//! leftmost nonzero component must match, everything right of it may be anything" — for a
//! pre-1.0 crate that component is the minor (CONTRACT.md's own 0.x convention everywhere
//! in this family), for a 1.0+ crate it's the major. One stdlib string split covers it.
//!
//! Regression shape: revert the README's pin to `"0.1"` and this goes red, because that
//! names minor 1 while the shipped crate is minor 3.

use std::path::Path;

fn readme() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Extract the version string from `commitward = "<req>"` inside the README's
/// `[dependencies]` code fence — the exact line a consumer copies.
fn readme_pin() -> String {
    let text = readme();
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("commitward = \""))
        .unwrap_or_else(|| panic!("README.md has no `commitward = \"...\"` line: {text:?}"));
    let start = line.find('"').expect("opening quote") + 1;
    let rest = &line[start..];
    let end = rest.find('"').expect("closing quote");
    rest[..end].to_string()
}

/// Parse a plain `major[.minor[.patch]]` string (no `^`/`~`/comparator prefix — neither the
/// README's pins nor `CARGO_PKG_VERSION` ever carry one) into its numeric components.
fn parse_version(s: &str) -> Vec<u64> {
    s.split('.')
        .map(|p| {
            p.parse::<u64>()
                .unwrap_or_else(|e| panic!("version component {p:?} in {s:?} is not a number: {e}"))
        })
        .collect()
}

/// Cargo's caret-requirement compatibility rule, restricted to the plain-number pins this
/// crate's README and `Cargo.toml` both use: for a 0.x crate the minor is the breaking
/// position and the pin must name and match it; for a 1.0+ crate the major is, and the
/// minor may be anything (a 1.x pin resolves to the latest 1.y).
fn pin_is_compatible(pin: &[u64], crate_version: &[u64]) -> bool {
    let crate_major = crate_version[0];
    if crate_major == 0 {
        pin.first() == Some(&0) && pin.get(1) == crate_version.get(1)
    } else {
        pin.first() == Some(&crate_major)
    }
}

#[test]
fn the_readme_pin_is_semver_compatible_with_the_shipped_crate() {
    let pin_str = readme_pin();
    let pin = parse_version(&pin_str);
    let crate_version_str = env!("CARGO_PKG_VERSION");
    let crate_version = parse_version(crate_version_str);
    assert!(
        pin_is_compatible(&pin, &crate_version),
        "README pins `commitward = \"{pin_str}\"`, which does not resolve to the shipped \
         crate version {crate_version_str} under cargo's caret rule — a consumer following \
         the README gets a different crate than the one that documents it (commitward#22)"
    );
}

#[test]
fn pin_compatibility_rule_matches_cargos_own_caret_semantics() {
    // 0.x: the minor is the breaking position — same minor, any patch, matches; a
    // different minor, even a newer one, does not (0.1 cannot pull in a 0.3 feature).
    assert!(pin_is_compatible(&[0, 3], &[0, 3, 2]));
    assert!(pin_is_compatible(&[0, 3, 0], &[0, 3, 2]));
    assert!(!pin_is_compatible(&[0, 1], &[0, 3, 2]));
    assert!(!pin_is_compatible(&[0, 4], &[0, 3, 2]));
    // 1.0+: the major is the breaking position — any minor/patch within it matches.
    assert!(pin_is_compatible(&[1], &[1, 9, 0]));
    assert!(pin_is_compatible(&[1, 0], &[1, 9, 0]));
    assert!(!pin_is_compatible(&[1], &[2, 0, 0]));
}
