//! `genmodel`: Rust-native Kurtosis config generation — the P2 replacement for
//! `scripts/generate_kurtosis_configs.py`.
//!
//! The config *bodies* (the helix YAML block, the commit-boost TOML block) are
//! ported VERBATIM from the Python templates into `const` strings: they are
//! already DRY there (helix is byte-identical across all 6 scenarios; CB is one
//! parameterized template), and the `{{ }}` runtime holes are filled by the
//! ethereum-package at launch, so they stay as literal text. Typing lives only
//! at the assembly layer (`Scenario` + `Images`) where it actually pays. See
//! `docs/plans/P2-consolidate-config-gen.md` for the grilled rationale.
//!
//! Task 0 lands only the golden-fixture regression harness; the generator bodies
//! (`helix`, `cb`, `scenario` submodules) land in Task 1.

pub mod cb;
pub mod helix;
pub mod scenario;

/// Extract a YAML `|` block scalar (named `key`, at 2-space indent) from `yaml`,
/// de-indented 4 spaces, with trailing blank lines removed. Test-only oracle
/// helper: it recovers the raw body the generator embeds so a body port can be
/// byte-compared against the golden independent of the surrounding assembly.
#[cfg(test)]
pub fn extract_block_scalar(yaml: &str, key: &str) -> String {
    let header = format!("  {key}: |");
    let mut lines = yaml.lines();
    for line in lines.by_ref() {
        if line == header {
            break;
        }
    }
    let mut body: Vec<String> = Vec::new();
    for line in lines {
        if line.is_empty() {
            body.push(String::new());
        } else if let Some(rest) = line.strip_prefix("    ") {
            body.push(rest.to_string());
        } else {
            // A less-indented non-empty line ends the block scalar.
            break;
        }
    }
    while body.last().is_some_and(|l| l.is_empty()) {
        body.pop();
    }
    body.join("\n")
}

/// The golden configs (one per scenario). The three single-relay ones (cb-basic,
/// cb-skip-sigverify, cb-extra-validation) are the exact Python output that
/// produced the green e2e run. The three multi-relay ones (cb-multiple-relays,
/// cb-timing-games, cb-mux) were REGENERATED for the intended two-Helix-instance
/// topology (the flashbots RELAY was dropped; the flashbots BUILDER stays), so
/// they are the intended output, not the old Python baseline. All are snapshotted
/// with the baked-default images. `sim generate` must reproduce each
/// byte-for-byte. Depth-independent path via `CARGO_MANIFEST_DIR`.
#[cfg(test)]
pub fn golden(scenario: &str) -> &'static str {
    match scenario {
        "cb-basic" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/golden-configs/cb-basic.yml"
        )),
        "cb-multiple-relays" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/golden-configs/cb-multiple-relays.yml"
        )),
        "cb-basic-nethermind-prysm" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/golden-configs/cb-basic-nethermind-prysm.yml"
        )),
        "cb-min-bid" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/golden-configs/cb-min-bid.yml"
        )),
        "cb-skip-sigverify" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/golden-configs/cb-skip-sigverify.yml"
        )),
        "cb-sigverify-diff" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/golden-configs/cb-sigverify-diff.yml"
        )),
        "cb-sigverify-diff-control" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/golden-configs/cb-sigverify-diff-control.yml"
        )),
        "cb-extra-validation" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/golden-configs/cb-extra-validation.yml"
        )),
        "cb-timing-games" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/golden-configs/cb-timing-games.yml"
        )),
        "cb-mux" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/golden-configs/cb-mux.yml"
        )),
        other => panic!("no golden fixture for scenario {other:?}"),
    }
}

/// Byte-diff `produced` against the golden for `scenario`. On mismatch, panics
/// naming the first differing line with a little context — the acceptance oracle
/// for the verbatim port (a byte-exact port makes byte-identity the right test).
#[cfg(test)]
pub fn assert_matches_golden(scenario: &str, produced: &str) {
    let expected = golden(scenario);
    if produced == expected {
        return;
    }
    let exp_lines: Vec<&str> = expected.lines().collect();
    let got_lines: Vec<&str> = produced.lines().collect();
    let max = exp_lines.len().max(got_lines.len());
    for i in 0..max {
        let e = exp_lines.get(i).copied();
        let g = got_lines.get(i).copied();
        if e != g {
            let ctx_start = i.saturating_sub(2);
            let mut ctx = String::new();
            for (j, line) in exp_lines.iter().enumerate().take(i).skip(ctx_start) {
                ctx.push_str(&format!("  {:>4}  {}\n", j + 1, line));
            }
            panic!(
                "{scenario}: output differs from golden at line {}:\n{ctx}  {:>4}- {}\n  {:>4}+ {}\n\
                 (expected {} lines, got {} lines)",
                i + 1,
                i + 1,
                e.unwrap_or("<missing>"),
                i + 1,
                g.unwrap_or("<missing>"),
                exp_lines.len(),
                got_lines.len(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [&str; 6] = [
        "cb-basic",
        "cb-multiple-relays",
        "cb-skip-sigverify",
        "cb-extra-validation",
        "cb-timing-games",
        "cb-mux",
    ];

    #[test]
    fn every_golden_matches_itself() {
        for s in ALL {
            assert_matches_golden(s, golden(s));
        }
    }

    #[test]
    #[should_panic(expected = "differs from golden at line")]
    fn a_flipped_line_is_caught() {
        // Flip one line of the basic golden; the harness must reject it.
        let mutated = golden("cb-basic").replacen("mev_type: custom", "mev_type: BOGUS", 1);
        assert_matches_golden("cb-basic", &mutated);
    }

    #[test]
    fn golden_images_are_the_baked_defaults() {
        // Guards hermeticity: the fixtures must encode the baked-default images
        // (the proven-good values), not a box-specific .env. Task 1's
        // Images::defaults() must reproduce exactly these.
        let basic = golden("cb-basic");
        assert!(basic.contains("mev_boost_image: commit-boost/commit-boost:kurtosis"));
        assert!(
            !basic.contains("commit-boost/pbs:kurtosis"),
            "the pbs bug must be gone"
        );
        assert!(basic.contains("helix_relay_image: ghcr.io/gattaca-com/helix-relay:main"));
    }
}
