# Plans index

Status classification of every `docs/plans/*` doc. A plan is a PROPOSAL that must EXPIRE: on ship it becomes
`landed` (history — extract durable WHY into site-comments, point at the implementing files). Ground steering
on `live` plans; treat `landed` ones as history.

| Plan | Status | What it delivered | Implementing files |
|---|---|---|---|
| [P1-sim-preflight-triage](P1-sim-preflight-triage.md) | **landed** (`a0c8be4`+) | `sim preflight` (real-image config validation, 3-valued verdict) + `sim triage` (root-cause from a broken enclave); the run-and-verify.sh preflight gate | `src/bin/sim/{preflight,triage,diagnose,render}.rs`, `scripts/run-and-verify.sh` |
| [P2-consolidate-config-gen](P2-consolidate-config-gen.md) | **landed** (`097a0b7`→`05a450f`) | Config generation ported to `sim generate` (typed Rust); Python generator + stale example deleted; `commit-boost/pbs`→`commit-boost/commit-boost` default fixed; `sim generate --check` drift gate | `src/bin/sim/{generate,genmodel/*}.rs`, `tests/fixtures/golden-configs/` |
| [P3-check-trustworthiness](P3-check-trustworthiness.md) | **landed** (`2868e8e`, `cad323a`, `145d7e1`) | The three false-green fixes (mux.routing WARN-gate, payload per-(relay,slot) conflict detection, best_bid v2 sourced from CB getHeader logs) — all live-devnet-validated | `src/checks/{mux_routing,payload_matching,best_bid}.rs` |

## Other steering docs (not plans)
- [SWEEP-BACKLOG.md](../SWEEP-BACKLOG.md) — **live** — the prioritized backlog from the 2026-08-01 six-lens
  sweep (bugs, docs, tests, perf, refactoring, features). The current source of "what's next."
- [NORTH-STAR.md](../NORTH-STAR.md) — durable WHY + design laws + the staged plan P0-P5. (The P0-P5 status
  ledger inside it lags reality; this INDEX + SWEEP-BACKLOG are the current status of record.)
- [ARCH.md](../ARCH.md) / [CHECKS.md](../CHECKS.md) / [fork-delta.md](../fork-delta.md) — reference (HOW it
  fits, the check catalog + verdict contract, the ethereum-package divergence).
- [local-kurtosis-e2e.md](../local-kurtosis-e2e.md) — the operational runbook (gotchas, kurtosis pin).

## Not-yet-written (candidates from the sweep)
- P4 (fork diet: upstream remote + tag pin + `bump` workflow), P5 (`sim run` — fold the shell launcher +
  orchestrator into Rust), and an ePBS first-slice plan. See SWEEP-BACKLOG.md "Features / roadmap".
