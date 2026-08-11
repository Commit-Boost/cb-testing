# ethereum-package fork delta

The vendored submodule at `ethereum-package/` is a fork of `ethpandaops/ethereum-package`
(fork origin: `github.com/Commit-Boost/ethereum-package`, currently detached at `fbe3141`).
The fork's own README / CHANGELOG / architecture read as stock upstream, so the divergence is
recoverable only from `git log`. This file makes that delta legible for a future rebase or
upstream PR. Cited against real commits and files; nothing here is committed by the doc itself.

## 1. Why the fork exists

Mainline `ethpandaops/ethereum-package` treats out-of-protocol block building as a bespoke,
hard-coded convenience. It has no incentive to keep commit-boost first-class, and **ePBS will
churn exactly this surface** (relay / builder / sidecar wiring). cb-testing owns the opinionated
block-building simulation substrate for commit-boost, so it maintains this fork opinionated about
commit-boost rather than waiting on upstream. See `docs/DESIGN.md` ("What cb-testing is") and Law 6.
The bet is explicitly "own it"; whether the fork investment is worth it long-term vs waiting on
ethpandaops#1384 is an open question — revisit if #1384 lands.

## 2. The delta, file by file

Fork-authored commits, newest first: `fbe3141` (N relay instances + 8GB cap), `43fe436`
(helix wait-for-genesis), `4844f88` + `1dbaa38` (disable zkboost), `022951e` (commit-boost
prometheus), `7efe6fe` (the custom-mev component model — the core IP). `1ecc324` is the last
`upstream/main` merge; `7efe6fe`'s parent `eac08c0` is upstream, so `7efe6fe` is the first fork
commit onto the upstream base.

| File | Commit | What changed | Why |
|---|---|---|---|
| `src/package_io/mev_resolver.star` (**new, 168 lines — the core IP**) | `7efe6fe` | Adds the `(relay, sidecar, builder)` component decomposition. `resolve_mev_components(mev_type, mev_params)` expands a preset `mev_type` via `MEV_PRESETS` OR, for `mev_type: "custom"`, reads explicit `mev_params.{mev_relay,mev_sidecar,mev_builder}`. Each component is independently `none`-able. Validates against `VALID_RELAYS/SIDECARS/BUILDERS`, normalizes relay to a list, and hard-errors impossible combos (e.g. `builder=flashbots` with all relays `none`). Helpers `get_sidecar_service_prefix`, `get_relay_image`. | Lets any relay × any sidecar × any builder mix without patching code (e.g. helix relay + commit-boost sidecar). Presets (`flashbots`/`helix`/`commit-boost`/`mev-rs`/`mock`/`buildoor`/`epbs`) stay as shortcuts; `custom` is the general API. |
| `main.star` | `7efe6fe` | Rewrote the MEV dispatch (~350 lines changed / net ~-43). Reads `args_with_right_defaults.mev_components`; a **relay-launch loop** iterates `mev_components.relay` with a `relay_index` counter, launching each relay service (helix/flashbots/mev-rs) at `index = num_participants + relay_index`; then a **per-validator sidecar loop** keyed on `mev_components.sidecar` (`mev-boost`/`commit-boost`/`mev-rs`/`none`); `builder == "buildoor"` and `sidecar == "none"` (ePBS) are branches. | Replaces the old single-hardcoded `mev_type` switch with the component model. The `num_participants + relay_index` suffix is the seam the 2-helix design relies on (§3). |
| `src/package_io/input_parser.star` | `7efe6fe` | Calls `mev_resolver.resolve_mev_components(...)` and threads `result["mev_components"]` through; surfaces `mev_relay`/`mev_sidecar`/`mev_builder` params. (~182 lines touched.) | Wires the resolver into the parsed args so `main.star` consumes a resolved struct, not raw `mev_type`. |
| `src/package_io/constants.star` | `7efe6fe` | Adds `CUSTOM_MEV_TYPE = "custom"`, `EPBS_MEV_TYPE = "epbs"`. `DEFAULT_HELIX_RELAY_IMAGE = "ghcr.io/gattaca-com/helix-relay:main"` (untagged `:main`). | New mev_type identifiers. The `:main` pin is why the helix config schema source-of-truth is the running binary's serde metadata, not any checked-in mirror (§4). |
| `src/mev/helix/helix_relay_launcher.star` | `43fe436`, then `fbe3141` | (a) **wait-for-genesis wrapper**: passes `GENESIS_TIME` env and sets `entrypoint=["sh","-c"]` + a cmd that `until [ "$(date +%s)" -ge "$GENESIS_TIME" ]; do sleep 1; done; exec /app/helix-relay --config ...`. (b) **N-instance suffixing**: service, postgres (`helix-relay-postgres-{index}`), and config-artifact names all suffixed by the per-instance `index`. (c) `RELAY_MAX_MEMORY` 4096 → **8192**. | (a) Latest `:main` helix **panics in `HousekeeperTile::new -> current_slot().unwrap()`** if it boots before genesis; the shell wrapper blocks until genesis (sh + date exist in image; `exec` preserves PID 1 / signals). (b) Lets two helix entries launch as `helix-relay-N`/`helix-relay-N+1` without name collision. (c) Both relays were **cgroup-OOM-killed (CONSTRAINT_MEMCG)** at the 4GB cap ~9min into a spamoor devnet; 8GB clears the window. |
| `src/mev/flashbots/mev_builder/mev_builder_launcher.star` | `fbe3141` | rbuilder config template now takes `participant_count`; emits **per-instance helix targets** with `Name=helix-{suffix}` / `Service=helix-relay-{suffix}` where `suffix = participant_count + relay_index`, mirroring `main.star`'s relay-launch loop verbatim (relay_index increments for every non-`none` relay). `Priority` keyed on `relay_index`. | flashbots rbuilder is kept as the BUILDER even when helix is the relay; its submission targets must resolve to the actually-launched helix service names. |
| `src/mev/commit-boost/mev_boost/mev_boost_launcher.star` | `022951e` (+ minor `7efe6fe`) | Adds a `metrics` port (9090), `CB_METRICS_PORT=9090` env. | Expose commit-boost prometheus metrics. |
| `static_files/mev/commit-boost/cb-config.toml.tmpl` | `022951e` | Adds `[metrics] enabled=true host="0.0.0.0" start_port=9090`. | Same — turn CB metrics on in the rendered config. |
| `main.star` (prometheus scrape jobs) | `022951e` | When `mev_components.sidecar == "commit-boost"`, appends a `commit-boost-{idx}` scrape job (`{ip}:9090/metrics`, 15s) per mev-boost context. | Prometheus actually scrapes the CB sidecars. |
| `main.star` (zkboost import + dispatch) | `1dbaa38`, `4844f88` | The `zkboost` import and its `additional_service == "zkboost"` launch branch are **commented out / stubbed** (`GpuConfig` is undefined on the upstream base). | zkboost is dead weight for this fork; kept commented (not deleted) to minimize rebase conflict churn. Flag for rebase: this is a stub, not a feature. |
| `static_files/mev/helix/config.yaml.tmpl` | `7efe6fe` | Trimmed (~-12 lines). | Config template reconciled with the `:main` helix serde layout. |
| `.github/tests/mev-custom-helix-cb.yaml` (**new**) | `7efe6fe` | Test scenario exercising `mev_type: custom` = helix relay + commit-boost sidecar. | Regression coverage for the component API. |
| `README.md`, `network_params.yaml`, `sanity_check.star`, `reth_launcher.star`, `participant_network.star` | `7efe6fe` | Doc/param/sanity plumbing for the new mev fields. | Supporting edits for the component model. |

## 3. The 2-helix design

The default topology drops the flashbots **relay** (its mev-boost-relay leaks ~825MB/min under
spamoor) and runs **two helix relays**, while still using flashbots **rbuilder** as the builder.
It works because everything downstream of the relay is positional and index-threaded:

- `main.star`'s relay-launch loop launches each non-`none` relay at
  `index = num_participants + relay_index`, so two `helix` entries in `mev_components.relay`
  become services `helix-relay-N` and `helix-relay-N+1` (with matching `helix-relay-postgres-N`
  and `helix-relay-config-N` artifacts) — no collision (`fbe3141` + `7efe6fe`).
- Ports / relay URLs / the commit-boost `[[relays]]` list / the mux routing were already
  positional, so no per-instance divergence is needed there.
- The **critical invariant**: `mev_builder_launcher.star` recomputes the exact same suffix
  (`participant_count + relay_index`, mirroring the launch loop) so the rbuilder `Service` names
  (`helix-relay-{suffix}`) resolve to the real launched services. If the two loops ever drift in
  how they count `relay_index` (note: a `mev-rs` relay still *consumes* an index slot but is not
  emitted into rbuilder config), block submission silently targets a nonexistent service.

Validated (`fbe3141` msg): the `cb-multiple-relays` devnet brings up `helix-relay-2` +
`helix-relay-3`, both survive, CB sees 2 relays (33 competitive bid slots).

## 4. Rebase / maintenance notes

- **No `upstream` remote is configured.** `git remote -v` shows only `origin =
  Commit-Boost/ethereum-package`. A rebase today has nothing to rebase against without first
  `git remote add upstream https://github.com/ethpandaops/ethereum-package`.
- **The fork is not tag-pinned.** The submodule is a bare detached HEAD at `fbe3141`; the many
  `git tag` entries are inherited upstream release tags, not a fork pin.
- **The planned fork diet + treadmill** wants an `upstream` remote + a tagged pin, and wants
  that pin moved **in lockstep with `HELIX_RELAY_IMAGE`** (`DEFAULT_HELIX_RELAY_IMAGE =
  ...helix-relay:main` in `constants.star`).
- **Schema source-of-truth for helix is the `:main` binary's serde metadata, not the fork
  checkout.** Helix types are not reusably importable (divergent branch / different org), and the
  helix config drifts against whatever `:main` currently deserializes — the wait-for-genesis and
  config-template fixes exist precisely because a checked-in mirror lags the deployed binary
  (DESIGN Law 1 caveat). Reconcile config changes by parsing against the actual
  image (the Preflight law), not by editing to match a stale local checkout.

## 5. What's already upstream-PR-shaped

**DESIGN Law 6** describes a **medium PR**: the `mev_resolver.star` component module + the
`main.star` mev-dispatch refactor + the `input_parser.star` per-client builder-flag matrix.
That code already exists here (`7efe6fe`) and cb-testing already consumes it, so upstreaming is
maturing-what-exists, not a new build — and there is no rush (Law 6, RATIFIED).

**Law 6b** — the external-builder hook + independent `none`-ability of each component — is
**already present**: `resolve_mev_components` lets `relay`/`sidecar`/`builder` each be `"none"`
independently (see `VALID_*` lists and the `epbs` preset `{relay:none, sidecar:none,
builder:buildoor}`), which is the missing piece Law 6b called out as the enabler for a future
thin compose-over-unmodified-upstream shim. What is *not* yet done: a true external-supplied
builder URL hook (VCs pointing at an arbitrary external builder) — `get_relay_image` / the
builder branches still resolve known images.

Note (Law 6): the `#1384` "exit" referenced in earlier audits is **unverified** — check upstream
HEAD before opening any PR, and confirm upstream still injects the VC `--builder` flag inside
`enrich_mev_extra_params` (the reason a pure shim isn't possible on today's upstream).
