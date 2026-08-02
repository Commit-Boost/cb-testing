//! `sim doctor` — host-prerequisite preflight for a Kurtosis devnet.
//!
//! Turns the hard-won gotchas in `docs/local-kurtosis-e2e.md` into one command:
//! is kurtosis installed (and the config-version-9-safe 1.18.1 pin), is docker
//! reachable, does the host have memory headroom for a ~10-min devnet, is the CB
//! image built, and is the forked `ethereum-package` submodule initialized.
//!
//! Structure mirrors the other verbs: a PURE classifier (`classify`, given probe
//! results -> verdict) that is unit-tested, and a thin IO layer (`gather_probes`)
//! that shells `kurtosis`/`docker` with `std::process::Command` and reads
//! `/proc/meminfo`. Sync only; no tokio.

use std::fs;
use std::path::Path;
use std::process::Command;

use eyre::Result;

/// The default CB image tag (matches `.env.example`); overridable via
/// `MEV_BOOST_IMAGE` in `.env`.
const DEFAULT_CB_IMAGE: &str = "commit-boost/commit-boost:kurtosis";

/// The kurtosis version this box is pinned to. A NEWER CLI writes a
/// `config-version: 9` config file that 1.18.1 can't read (the clash documented
/// in `docs/local-kurtosis-e2e.md`), so anything but this is a WARN.
const PINNED_KURTOSIS: (u64, u64, u64) = (1, 18, 1);

/// Memory a devnet wants available (MemAvailable + SwapFree), in MB. The task
/// floor is ~18GB; `scripts/run-and-verify.sh` uses 24000 with more headroom.
const NEED_MB: u64 = 18_000;

/// Per-item verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn glyph(self) -> char {
        match self {
            Status::Ok => '\u{2713}',   // ✓
            Status::Warn => '\u{26a0}', // ⚠
            Status::Fail => '\u{2717}', // ✗
        }
    }
}

/// One checklist line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: &'static str,
    pub status: Status,
    /// Whether this is a HARD prerequisite (a Fail exits nonzero). Only kurtosis
    /// and docker are hard; memory/image/submodule are advisory warnings.
    pub hard: bool,
    pub detail: String,
}

/// The raw probe results — the ONLY input to the pure classifier. `gather_probes`
/// fills this from the host; tests construct it directly.
#[derive(Debug, Clone)]
pub struct Probes {
    /// Raw `kurtosis version` output, or `None` if the CLI is not installed.
    pub kurtosis_version_raw: Option<String>,
    /// `docker info` returned exit 0.
    pub docker_ok: bool,
    /// `/proc/meminfo` MemAvailable, in MB.
    pub mem_available_mb: u64,
    /// `/proc/meminfo` SwapFree, in MB.
    pub swap_free_mb: u64,
    /// The CB image tag we looked for (from `.env` or the default).
    pub cb_image: String,
    /// `docker image inspect <cb_image>` returned exit 0.
    pub cb_image_present: bool,
    /// The `ethereum-package` submodule directory is initialized (non-empty).
    pub submodule_initialized: bool,
}

/// The full classified report.
#[derive(Debug, Clone)]
pub struct Report {
    pub items: Vec<Item>,
    /// 0 = all hard prerequisites present; 1 = a hard prerequisite is missing.
    pub exit_code: i32,
}

/// Parse a `(major, minor, patch)` semver out of arbitrary text (e.g.
/// `"CLI Version:   1.18.1"`). Returns the first `N.N.N` run found.
pub fn parse_semver(text: &str) -> Option<(u64, u64, u64)> {
    for token in text.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        let mut parts = token.split('.');
        let (Some(a), Some(b), Some(c)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if parts.next().is_some() {
            continue; // more than 3 components — not a plain semver
        }
        if let (Ok(a), Ok(b), Ok(c)) = (a.parse(), b.parse(), c.parse()) {
            return Some((a, b, c));
        }
    }
    None
}

/// The PURE decision: probe results -> checklist + exit code. No IO.
pub fn classify(p: &Probes) -> Report {
    let mut items = Vec::new();

    // (a) kurtosis installed + version pin. Missing = hard fail.
    items.push(match &p.kurtosis_version_raw {
        None => Item {
            name: "kurtosis CLI",
            status: Status::Fail,
            hard: true,
            detail: "not installed / not on PATH — install kurtosis-cli (see docs/local-kurtosis-e2e.md Step 0)".to_string(),
        },
        Some(raw) => match parse_semver(raw) {
            Some(v) if v == PINNED_KURTOSIS => Item {
                name: "kurtosis CLI",
                status: Status::Ok,
                hard: true,
                detail: format!("{}.{}.{} (pinned)", v.0, v.1, v.2),
            },
            Some(v) if v > PINNED_KURTOSIS => Item {
                name: "kurtosis CLI",
                status: Status::Warn,
                hard: true,
                detail: format!(
                    "{}.{}.{} is newer than the pinned {}.{}.{} — risks the config-version-9 clash (1.18.1 can't read it); see docs/local-kurtosis-e2e.md",
                    v.0, v.1, v.2, PINNED_KURTOSIS.0, PINNED_KURTOSIS.1, PINNED_KURTOSIS.2,
                ),
            },
            Some(v) => Item {
                name: "kurtosis CLI",
                status: Status::Warn,
                hard: true,
                detail: format!(
                    "{}.{}.{} is older than the pinned {}.{}.{} — untested here",
                    v.0, v.1, v.2, PINNED_KURTOSIS.0, PINNED_KURTOSIS.1, PINNED_KURTOSIS.2,
                ),
            },
            None => Item {
                name: "kurtosis CLI",
                status: Status::Warn,
                hard: true,
                detail: format!("installed, but could not parse version from {raw:?}"),
            },
        },
    });

    // (b) docker daemon reachable. Missing = hard fail.
    items.push(if p.docker_ok {
        Item {
            name: "docker daemon",
            status: Status::Ok,
            hard: true,
            detail: "reachable (`docker info` ok)".to_string(),
        }
    } else {
        Item {
            name: "docker daemon",
            status: Status::Fail,
            hard: true,
            detail: "unreachable — `docker info` failed; is the daemon running and are you in the docker group?".to_string(),
        }
    });

    // (c) host memory headroom. Advisory warning only.
    let usable = p.mem_available_mb + p.swap_free_mb;
    items.push(if usable >= NEED_MB {
        Item {
            name: "host memory",
            status: Status::Ok,
            hard: false,
            detail: format!(
                "{usable}MB usable (avail {} + swapfree {}) >= {NEED_MB}MB",
                p.mem_available_mb, p.swap_free_mb
            ),
        }
    } else {
        Item {
            name: "host memory",
            status: Status::Warn,
            hard: false,
            detail: format!(
                "only {usable}MB usable (avail {} + swapfree {}) < {NEED_MB}MB — a devnet may stall or thrash",
                p.mem_available_mb, p.swap_free_mb
            ),
        }
    });

    // (d) CB image present. Advisory warning only.
    items.push(if p.cb_image_present {
        Item {
            name: "CB image",
            status: Status::Ok,
            hard: false,
            detail: format!("{} present", p.cb_image),
        }
    } else {
        Item {
            name: "CB image",
            status: Status::Warn,
            hard: false,
            detail: format!(
                "{} not found locally — build it (`just build-all kurtosis`) or set MEV_BOOST_IMAGE in .env",
                p.cb_image
            ),
        }
    });

    // (e) ethereum-package submodule initialized. Advisory warning only.
    items.push(if p.submodule_initialized {
        Item {
            name: "ethereum-package submodule",
            status: Status::Ok,
            hard: false,
            detail: "initialized (non-empty)".to_string(),
        }
    } else {
        Item {
            name: "ethereum-package submodule",
            status: Status::Warn,
            hard: false,
            detail: "empty — run `git submodule update --init --recursive`".to_string(),
        }
    });

    let exit_code = if items.iter().any(|i| i.hard && i.status == Status::Fail) {
        1
    } else {
        0
    };
    Report { items, exit_code }
}

/// Entry point for `sim doctor`. Probes the host, prints the checklist, and
/// exits nonzero if a HARD prerequisite (kurtosis, docker) is missing.
pub fn run() -> Result<()> {
    let probes = gather_probes();
    let report = classify(&probes);

    println!("sim doctor — devnet host preflight");
    println!();
    for item in &report.items {
        println!("  {} {}: {}", item.status.glyph(), item.name, item.detail);
    }
    println!();
    if report.exit_code == 0 {
        println!("hard prerequisites OK (warnings above are advisory).");
    } else {
        println!("MISSING a hard prerequisite (kurtosis / docker) — fix the ✗ items above.");
    }

    if report.exit_code != 0 {
        std::process::exit(report.exit_code);
    }
    Ok(())
}

/// The IO layer: run the shell probes + read `/proc/meminfo`. Best-effort — a
/// failed probe becomes a negative/absent result, never a panic.
fn gather_probes() -> Probes {
    let (mem_available_mb, swap_free_mb) = read_meminfo(Path::new("/proc/meminfo"));
    let cb_image = cb_image_from_env(Path::new(".env"));
    Probes {
        kurtosis_version_raw: probe_kurtosis_version(),
        docker_ok: cmd_ok("docker", &["info"]),
        mem_available_mb,
        swap_free_mb,
        cb_image_present: cmd_ok("docker", &["image", "inspect", &cb_image]),
        cb_image,
        submodule_initialized: dir_non_empty(Path::new("ethereum-package")),
    }
}

/// `kurtosis version` stdout, or `None` if the binary isn't runnable.
fn probe_kurtosis_version() -> Option<String> {
    let out = Command::new("kurtosis").arg("version").output().ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// True iff `<prog> <args...>` spawned and exited 0.
fn cmd_ok(prog: &str, args: &[&str]) -> bool {
    Command::new(prog)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Read `MemAvailable` + `SwapFree` (KB) from a `/proc/meminfo`-shaped file, in
/// MB. A missing file / field reads as 0 (which will trip the low-memory WARN).
fn read_meminfo(path: &Path) -> (u64, u64) {
    let contents = fs::read_to_string(path).unwrap_or_default();
    let field_mb = |key: &str| -> u64 {
        contents
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| kb / 1024)
            .unwrap_or(0)
    };
    (field_mb("MemAvailable:"), field_mb("SwapFree:"))
}

/// The CB image tag from `.env` (`MEV_BOOST_IMAGE`), else the default.
fn cb_image_from_env(env_path: &Path) -> String {
    let Ok(contents) = fs::read_to_string(env_path) else {
        return DEFAULT_CB_IMAGE.to_string();
    };
    for line in contents.lines() {
        let stripped = line.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = stripped.split_once('=')
            && key.trim() == "MEV_BOOST_IMAGE"
        {
            let v = value.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    DEFAULT_CB_IMAGE.to_string()
}

/// True iff `path` is a directory with at least one entry.
fn dir_non_empty(path: &Path) -> bool {
    fs::read_dir(path)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_probes() -> Probes {
        Probes {
            kurtosis_version_raw: Some("CLI Version:   1.18.1\n".to_string()),
            docker_ok: true,
            mem_available_mb: 30_000,
            swap_free_mb: 0,
            cb_image: DEFAULT_CB_IMAGE.to_string(),
            cb_image_present: true,
            submodule_initialized: true,
        }
    }

    #[test]
    fn parse_semver_extracts_the_version() {
        assert_eq!(parse_semver("CLI Version:   1.18.1"), Some((1, 18, 1)));
        assert_eq!(parse_semver("kurtosis version 1.20.0\n"), Some((1, 20, 0)));
        assert_eq!(parse_semver("no version here"), None);
        // A 4-component build string is not a plain semver.
        assert_eq!(parse_semver("1.2.3.4"), None);
    }

    #[test]
    fn all_healthy_exits_zero_and_all_ok() {
        let report = classify(&healthy_probes());
        assert_eq!(report.exit_code, 0);
        assert!(report.items.iter().all(|i| i.status == Status::Ok));
        assert_eq!(report.items.len(), 5);
    }

    #[test]
    fn missing_kurtosis_is_a_hard_fail() {
        let mut p = healthy_probes();
        p.kurtosis_version_raw = None;
        let report = classify(&p);
        assert_eq!(report.exit_code, 1);
        let k = &report.items[0];
        assert_eq!(k.name, "kurtosis CLI");
        assert_eq!(k.status, Status::Fail);
        assert!(k.hard);
    }

    #[test]
    fn missing_docker_is_a_hard_fail() {
        let mut p = healthy_probes();
        p.docker_ok = false;
        assert_eq!(classify(&p).exit_code, 1);
    }

    #[test]
    fn newer_kurtosis_warns_but_is_not_fatal() {
        let mut p = healthy_probes();
        p.kurtosis_version_raw = Some("1.20.0".to_string());
        let report = classify(&p);
        assert_eq!(
            report.exit_code, 0,
            "a version warning must not fail the run"
        );
        assert_eq!(report.items[0].status, Status::Warn);
        assert!(report.items[0].detail.contains("config-version-9"));
    }

    #[test]
    fn low_memory_warns_but_does_not_fail() {
        let mut p = healthy_probes();
        p.mem_available_mb = 4_000;
        p.swap_free_mb = 1_000;
        let report = classify(&p);
        assert_eq!(report.exit_code, 0);
        let mem = report
            .items
            .iter()
            .find(|i| i.name == "host memory")
            .unwrap();
        assert_eq!(mem.status, Status::Warn);
        assert!(!mem.hard);
    }

    #[test]
    fn swap_counts_toward_memory_headroom() {
        let mut p = healthy_probes();
        p.mem_available_mb = 10_000;
        p.swap_free_mb = 10_000; // 20_000 >= NEED_MB
        let mem = classify(&p)
            .items
            .into_iter()
            .find(|i| i.name == "host memory")
            .unwrap();
        assert_eq!(mem.status, Status::Ok);
    }

    #[test]
    fn missing_image_and_submodule_are_warnings_only() {
        let mut p = healthy_probes();
        p.cb_image_present = false;
        p.submodule_initialized = false;
        let report = classify(&p);
        assert_eq!(report.exit_code, 0, "image/submodule are advisory");
        assert!(
            report
                .items
                .iter()
                .filter(|i| i.status == Status::Warn)
                .count()
                >= 2
        );
    }

    #[test]
    fn read_meminfo_parses_kb_to_mb() {
        let dir = std::env::temp_dir().join(format!("sim-doctor-mem-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("meminfo");
        fs::write(
            &f,
            "MemTotal:       65808360 kB\nMemAvailable:   20480000 kB\nSwapTotal:      8000000 kB\nSwapFree:        1048576 kB\n",
        )
        .unwrap();
        let (avail, swap) = read_meminfo(&f);
        assert_eq!(avail, 20_000); // 20480000 / 1024
        assert_eq!(swap, 1024); // 1048576 / 1024
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cb_image_from_env_reads_override_else_default() {
        let dir = std::env::temp_dir().join(format!("sim-doctor-env-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let env = dir.join(".env");
        fs::write(&env, "# c\nMEV_BOOST_IMAGE=my/cb:tag\n").unwrap();
        assert_eq!(cb_image_from_env(&env), "my/cb:tag");
        assert_eq!(
            cb_image_from_env(Path::new("/no/such/.env")),
            DEFAULT_CB_IMAGE
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
