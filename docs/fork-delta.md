# ethereum-package patch set

The `ethereum-package/` submodule tracks branch `cb-on-upstream` on
`github.com/Commit-Boost/ethereum-package`: upstream `ethpandaops/ethereum-package` at a pinned
commit plus the small set of commits below. It is a patch branch, not a fork. Upstream's own
`mev_type: commit-boost` launches the CB sidecar and the flashbots (reth-rbuilder) builder; the
patches add what our scenarios need on top, and fix what stock upstream cannot launch today.

Base: upstream `6dd3f26`. Every config in `sweep-gate`, plus `cb-signer`, passes against it.

## The patches, oldest first

| Commit | What | Why stock upstream is not enough |
|---|---|---|
| `03ab0f1` | `mev_params.helix_relay_config`: an inline Helix config replacing the static template | The static template is stale for current helix, and scenarios need their own route and timing settings |
| `22d4b4c` | Helix starts and stays up: `POSTGRES_PASSWORD` and `ADMIN_TOKEN` env, an entrypoint that waits for genesis, 8GB memory, 2GB `/dev/shm`, the port check on the admin port | Current helix panics without the env vars, panics if it boots before genesis, is OOM-killed at 4GB under spamoor, SIGBUSes on docker's 64MB shm, and never opens the website port a trimmed config drops |
| `73f4cb9` | The builder's reth gets `enode` bootnodes | Upstream passes ENRs, which `ethpandaops/reth-rbuilder:develop` (reth 2.2.0-dev, the newest tag) rejects, so the builder never starts |
| `182603a` | The CB sidecar sets `CB_METRICS_PORT` and publishes a `metrics` port | Without it Commit-Boost serves no metrics and every `cb_*` check skips, so a run is green having measured nothing |
| `617b8c0` | `mev_params.mev_relays`: a list of relay kinds that replaces the relay set | Upstream cannot run two helix relays, and always brings the flashbots relay stack with `run_multiple_relays` |
| `2b4cd15` | `mev_builder_subsidy` may be a list, one value per relay | Two relays fed identical bids leave the best-bid check nothing to tell apart |
| `d160107` | `mev_params.commit_boost_extra_files`, rendered next to `cb-config.toml` | The file-sourced relay API key scenario needs a file in the sidecar's `/config` |
| `efdbcd6` | The builder's reth also gets `--trusted-peers` | Upstream runs nethermind discv5-only and this reth finds bootnodes over discv4 only, so a builder paired with nethermind had no EL peers, an empty mempool, and never submitted a block |
| `12e96e7` | `mev_params.commit_boost_signer` launches a Commit-Boost signer beside each sidecar | Upstream has no signer service |

## The one invariant across patches

`mev_relays` names relay *i* `helix-relay-{num_participants + i}`, and the rbuilder config
template recomputes the same index to address each relay. If the two ever count differently,
block submission silently targets a service that does not exist. cb-testing relies on the name:
the poisoned-relay scenario addresses `helix-relay-2` directly, which is why a single relay is
still listed through `mev_relays`.

## Maintaining it

- **Rebase, don't layer.** Move to a new upstream by rebasing `cb-on-upstream` onto it, then run
  `just sweep-gate`. Drop any patch upstream has made redundant rather than keeping it around.
- **A patch earns its place with a failing run.** Each row above says what broke without it. A
  new patch should arrive the same way, and leave once its "why" stops being true.
- **The `--target fork` opt-in** still generates configs for the old fork's `mev_type: custom`
  shape, for comparison only. The old fork lives on the same remote as branch `main`.
- **Helix's config schema comes from the running image**, not a checked-in copy: the helix
  config is checked against the actual image (the Preflight law), so a drifting `develop`
  build fails loudly at preflight.

## Upstream candidates

These are bug fixes against upstream as it is, and would read as normal PRs: `22d4b4c` (current
helix cannot start), `73f4cb9` and `efdbcd6` (the flashbots builder cannot join a devnet), and
`182603a` (the CB sidecar exposes no metrics). The rest are features for our scenarios.
