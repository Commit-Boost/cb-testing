//! `sim generate` — write Kurtosis args-files for the CB test scenarios.
//!
//! The pure assembly lives in `genmodel::scenario`; this module is the IO
//! boundary: it applies `.env` image overrides (read-only), resolves the output
//! directory, and writes `configs/generated/<scenario>.yml`. Applying overrides
//! HERE (not inside `args_file`) keeps assembly pure and hermetically testable
//! against the golden fixtures.

use std::fs;
use std::path::Path;

use eyre::{Result, WrapErr, eyre};

use crate::genmodel::scenario::{Images, Scenario};

/// Generate one scenario (by name) or all six (`None`) into `out_dir`. Reads
/// `keys/` and `.env` relative to the CWD (the repo root — how `just
/// generate-configs` and the Python generator both run).
pub fn run(scenario: Option<&str>, out_dir: &Path, curated: bool) -> Result<()> {
    run_in(
        scenario,
        out_dir,
        Path::new("keys"),
        Path::new(".env"),
        curated,
    )
}

/// Testable core with the two IO roots injected. Assembles ALL bodies (reading +
/// validating the mux key files) BEFORE writing anything, so a missing/malformed
/// keys file fails cleanly with nothing written — no partial output. This mirrors
/// the Python's pre-write `load_pubkeys` + `sys.exit(1)` all-or-nothing contract.
fn run_in(
    scenario: Option<&str>,
    out_dir: &Path,
    keys_dir: &Path,
    env_path: &Path,
    curated: bool,
) -> Result<()> {
    let images = images_from_env(env_path);
    let outputs = assemble(scenario, &images, keys_dir, curated)?;

    fs::create_dir_all(out_dir)
        .wrap_err_with(|| format!("creating output dir {}", out_dir.display()))?;

    for (name, body) in &outputs {
        let path = out_dir.join(format!("{name}.yml"));
        fs::write(&path, body).wrap_err_with(|| format!("writing {}", path.display()))?;
        tracing::info!(scenario = name, path = %path.display(), "generated config");
        println!("Generated {}", path.display());
    }

    Ok(())
}

/// Verify the on-disk configs already match what the generator would produce,
/// WITHOUT writing (CI / agent drift gate). Errors (nonzero exit) on any drift.
pub fn check(scenario: Option<&str>, out_dir: &Path, curated: bool) -> Result<()> {
    check_in(
        scenario,
        out_dir,
        Path::new("keys"),
        Path::new(".env"),
        curated,
    )
}

fn check_in(
    scenario: Option<&str>,
    out_dir: &Path,
    keys_dir: &Path,
    env_path: &Path,
    curated: bool,
) -> Result<()> {
    let images = images_from_env(env_path);
    let outputs = assemble(scenario, &images, keys_dir, curated)?;

    let mut drift: Vec<String> = Vec::new();
    for (name, body) in &outputs {
        let path = out_dir.join(format!("{name}.yml"));
        match fs::read_to_string(&path) {
            Ok(on_disk) if &on_disk == body => println!("ok    {}", path.display()),
            Ok(_) => drift.push(format!("{} differs from `sim generate`", path.display())),
            Err(_) => drift.push(format!("{} missing (would be created)", path.display())),
        }
    }

    if drift.is_empty() {
        Ok(())
    } else {
        Err(eyre!(
            "{} config(s) out of date — run `just generate-configs`:\n  {}",
            drift.len(),
            drift.join("\n  ")
        ))
    }
}

/// Select scenarios and assemble each `(name, body)`. Reads mux key files (the
/// only fallible step) — shared by `run` and `check` so both fail identically
/// before touching the filesystem.
fn assemble(
    scenario: Option<&str>,
    images: &Images,
    keys_dir: &Path,
    curated: bool,
) -> Result<Vec<(String, String)>> {
    let scenarios: Vec<Scenario> = match scenario {
        Some(name) => vec![
            Scenario::from_name(name)
                .ok_or_else(|| eyre!("unknown scenario {name:?}; expected one of {:?}", names()))?,
        ],
        None => Scenario::ALL.to_vec(),
    };
    let mut out: Vec<(String, String)> = scenarios
        .iter()
        .map(|s| Ok((s.name().to_string(), s.args_file_in(images, keys_dir)?)))
        .collect::<Result<_>>()?;
    // The curated composable coverage points (rendered from ScenarioSpec, not
    // the Scenario enum). `--curated` emits them alongside the named scenarios.
    if curated {
        for (name, spec) in crate::genmodel::spec::curated() {
            out.push((
                name.to_string(),
                spec.render(&spec.auto_comment(), images, keys_dir)?,
            ));
        }
    }
    Ok(out)
}

fn names() -> Vec<&'static str> {
    Scenario::ALL.iter().map(|s| s.name()).collect()
}

/// Build the image map from defaults, overridden by `.env` if present. Mirrors
/// the Python `.env` key names. A missing `.env` is not an error (defaults win).
pub(crate) fn images_from_env(env_path: &Path) -> Images {
    let mut images = Images::default();
    let Ok(contents) = fs::read_to_string(env_path) else {
        return images;
    };
    for (key, value) in parse_env(&contents) {
        match key.as_str() {
            "HELIX_RELAY_IMAGE" => images.helix_relay = value,
            "MEV_RELAY_IMAGE" => images.mev_relay = value,
            "MEV_BOOST_IMAGE" => images.mev_boost = value,
            "BUILDER_EL_IMAGE" => images.builder_el = value,
            "BUILDER_CL_IMAGE" => images.builder_cl = value,
            _ => {}
        }
    }
    images
}

/// Parse `KEY=VALUE` pairs, skipping blanks and `#` comments. No quoting/expansion
/// (matches the Python `load_env`).
fn parse_env(contents: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let stripped = line.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = stripped.split_once('=') {
            out.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_overrides_apply_over_defaults() {
        let dir = std::env::temp_dir().join(format!("sim-env-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let env = dir.join(".env");
        fs::write(
            &env,
            "# c\nHELIX_RELAY_IMAGE=custom/helix:dev\n\nMEV_BOOST_IMAGE=x/y:z\n",
        )
        .unwrap();
        let images = images_from_env(&env);
        assert_eq!(images.helix_relay, "custom/helix:dev");
        assert_eq!(images.mev_boost, "x/y:z");
        // Untouched keys keep defaults.
        assert_eq!(images.builder_cl, "sigp/lighthouse:latest");
    }

    #[test]
    fn missing_env_yields_defaults() {
        let images = images_from_env(Path::new("/no/such/.env"));
        assert_eq!(images.helix_relay, Images::default().helix_relay);
    }

    /// IO faithfulness: `run` writes all six files, each byte-equal to the pure
    /// assembly for the resolved image set. (The hermetic GOLDEN byte-match lives
    /// in `scenario.rs::every_scenario_matches_its_golden`, on default images;
    /// this only proves the IO layer writes what assembly produced.)
    #[test]
    fn run_writes_all_six_matching_assembly() {
        let dir = std::env::temp_dir().join(format!("sim-gen-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        run(None, &dir, false).expect("generate all");
        let images = images_from_env(Path::new(".env"));
        for s in Scenario::ALL {
            let produced = fs::read_to_string(dir.join(format!("{}.yml", s.name()))).unwrap();
            let expected = s.args_file_in(&images, Path::new("keys")).unwrap();
            assert_eq!(produced, expected, "{} on-disk body", s.name());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// `--check` passes when on-disk configs match the generator, and fails
    /// (Err → nonzero exit) when one has drifted.
    #[test]
    fn check_passes_when_current_and_fails_on_drift() {
        let dir = std::env::temp_dir().join(format!("sim-check-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        run(None, &dir, false).expect("seed");
        // Fresh output → check is clean.
        check(None, &dir, false).expect("check should pass on freshly-generated configs");
        // Mutate one file → check must fail.
        let f = dir.join("cb-basic.yml");
        let mut body = fs::read_to_string(&f).unwrap();
        body.push_str("\n# hand-edit\n");
        fs::write(&f, body).unwrap();
        let err = check(None, &dir, false).unwrap_err();
        assert!(err.to_string().contains("out of date"), "got: {err}");
        assert!(
            err.to_string().contains("cb-basic.yml"),
            "names the drifted file: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Atomicity: a missing keys dir (mux can't load pubkeys) must fail with
    /// NOTHING written — the output dir is not even created.
    #[test]
    fn run_is_atomic_on_missing_keys() {
        let dir = std::env::temp_dir().join(format!("sim-atomic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let err = run_in(
            None,
            &dir,
            Path::new("/no/such/keys"),
            Path::new("/no/such/.env"),
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("pubkey file"), "got: {err}");
        assert!(!dir.exists(), "no output dir should be created on failure");
    }
}
