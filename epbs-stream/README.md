# epbs-stream: live test of Commit-Boost's ePBS bid stream against Helix

This harness tests Commit-Boost (CB) racing its HTTP `getExecutionPayloadBid` request
against Helix's websocket bid stream,
`GET /eth/v1/builder/execution_payload_bid_stream/{slot}/{parent_hash}/{parent_root}/{proposer_pubkey}`,
on a real gloas devnet.

```
lodestar VC --(keymanager builder_config)--> cb-epbs (get_header = "stream")
                                               |  HTTP getExecutionPayloadBid  \
                                               |  websocket bid stream          > helix-relay -> helix-simstub
                                                                                     ^
                               gloas_inject --(SSZ SignedBidSubmissionGloas)---------+
```

- **Devnet:** cb-testing's `configs/epbs/gloas-epbs.yaml`, rendered with `preset: mainnet` and
  `seconds_per_slot: 12`, launched with upstream ethereum-package (`EP_PACKAGE`). It runs
  lodestar `local/lodestar:km` as the CL and VC, with 64 validators and gloas at genesis.
- **Helix:** a release build of the ref under test. It runs with `is_local_dev` (no Postgres),
  `header_stream.admit_all: true` and `stream_for_ms: 300`. The synthetic builder key is listed
  in `builders`. The simulator is `stub/simstub.py`, which accepts every block.
- **CB:** `CB_IMAGE`, using `config/cb-config.toml.tmpl`. This is cb-testing's ePBS template
  with buildoor replaced by `helix` / `helix-mux` entries that set `get_header = "stream"`, and
  stdout logging at debug for the `cb_*` crates. `cb-km apply` points every validator's
  builder_config at CB.
- **Injector:** `gloas_inject`, a test-harness bin on Helix branch `harness/gloas-inject`.
  It follows the BN's `payload_attributes` SSE and POSTs 8 rising bids per slot (0.05 to
  0.12 ETH) at -1500, -500, +100, +300, +500, +1000, +1100 and +1200 ms from the slot start.
  The last three land inside Helix's 300 ms stream window. The payloads are synthetic
  (random block hash, no transactions) but carry the live parent hash, prev_randao,
  timestamp and withdrawals.

Why 12 s slots: lodestar:km reports `SECONDS_PER_SLOT: 12` whatever the genesis says. With
6 s slots, `SLOT_DURATION_MS: 6000` disagrees with it, lighthouse's
`ChainSpec::from_config` rejects the spec, and Helix panics at startup. `up.sh` checks this.

## Prerequisites

- `kurtosis` (engine running), `docker`, `python3`, a Rust toolchain via rustup, and about
  15 GB of free RAM.
- Local images: `local/lodestar:km` (cb-testing's ePBS lodestar), `ubuntu:24.04`,
  `nicolaka/netshoot` (packet capture), and the CB image under test.
- `cb-km` built from the same CB checkout as `CB_IMAGE` (`cargo build -p cb-km-tool --release`).
  Pass its path as `CB_KM_BIN` unless it is on `PATH`.
- A Helix clone that holds both the ref under test and `harness/gloas-inject`. The default is
  `../../helix-epbs-stream` relative to this directory; override it with `HELIX_REPO`.

## Steps

```bash
cd epbs-stream
./build-images.sh feat/epbs-bid-stream        # any Helix ref; writes run/images.env
CB_KM_BIN=/path/to/cb-km ./up.sh              # devnet + simstub + helix + CB + injector
./scenario.sh                                 # bids, no bids, bids; capture; negative.sh; analyze
./down.sh                                     # remove the enclave and the capture sidecar
```

`build-images.sh <helix-ref> [inject-ref]` builds `helix-relay` from `<helix-ref>` and
`gloas_inject` from the `crates/test-harness` of `[inject-ref]` (default `harness/gloas-inject`),
laid over the same tree so that both use one set of types. It works in a shared clone under
`run/build/` and only reads `HELIX_REPO`. The images are
`local/helix-relay:epbs-stream-<sha8>`, `local/helix-inject:epbs-stream-<sha8>` and
`local/helix-simstub:epbs-stream`. Each carries `epbs-stream.helix-commit` and
`epbs-stream.inject-commit` labels, and the relay's `info` metric reports the commit it was
built from.

`up.sh` runs in stages: `launch`, `services`, `inject`, `relay` and `cb`. `relay` swaps
`helix-relay` for `$HELIX_IMAGE` and keeps its config, for an A/B test of a Helix change on
the same devnet. `cb` does the same for `cb-epbs` with `$CB_IMAGE`, then re-runs `cb-km apply`
with `$CB_KM_BIN`, for an A/B test of a CB change. To start or stop the bids by hand, run
`kurtosis service stop|start epbs-stream helix-inject`.

`negative.sh` sends hand-made websocket upgrades with a bad `X-Request-Auth` (missing, not
base64, over the 5612-char cap, signed for another slot, a corrupted or a non-proposer signature)
or a path pubkey that is not the slot's proposer. It exits non-zero unless all seven are refused.

### Settings (env)

| var | default | |
|---|---|---|
| `RUN_DIR` | `./run` (gitignored) | rendered configs, build tree, scenario output |
| `ENCLAVE` | `epbs-stream` | |
| `HELIX_IMAGE`, `INJECT_IMAGE`, `STUB_IMAGE` | from `run/images.env` | |
| `CB_IMAGE` | `commit-boost/commit-boost:optiond-21fcef9` | CB `feat/epbs-bid-stream` @ 21fcef9 |
| `CB_KM_BIN` | `cb-km` on `PATH` | |
| `HELIX_REPO` | `../../helix-epbs-stream` | |
| `EP_PACKAGE` | `ethereum-package@6dd3f26` | |
| `DEVNET_ARGS` | `../configs/epbs/gloas-epbs.yaml` | |
| `SECONDS_PER_SLOT` | `12` | |
| `STREAM_FOR_MS` | `300` | Helix `header_stream.stream_for_ms` |
| `INJECT_ARGS` | the 8-offset schedule above, `--duration-s 7200` | |
| `BID_SLOTS`, `NOBID_SLOTS`, `RESUME_SLOTS` | `6`, `12`, `4` | scenario lengths |

### Keys

`keys/` holds throwaway devnet keys, checked in on purpose. They sign nothing outside a
local devnet. Each secret key is `sha256("epbs-stream devnet <builder|relay> key")` with the
first byte reduced to `b % 0x60 + 1`, which keeps it below the BLS group order. Each
public key was printed by `gloas_inject --print-pubkey`. The relay key signs Helix's
responses (its pubkey is in CB's relay URL). The builder key signs the injected
submissions (its pubkey is in Helix's `builders`).

## Pass criteria

`scenario.sh` places each boundary `EDGE_MS` (3 s) into a slot. That is after the slot's
bid request has been answered and before the injector's first bid for the next slot. The
injector runs as PID 1, which ignores SIGTERM, so `kurtosis service stop` returns only
after Docker's 10 s timeout. The bid stretch therefore ends in the slot where the stop
returns. `scenario.sh` snapshots CB's metrics at every boundary and runs `tcpdump` in
cb-epbs's network namespace. `analyze.py` prints a per-slot table and these verdicts:

- **a. No-bid slots** (at least 10, all answered 204 by CB): 0 `Connection reset without
  closing handshake` and 0 `stream failed` lines in CB's log.
  `cb_pbs_relay_stream_fallback_total{endpoint="get_execution_payload_bid_stream"}` and the
  stream endpoint's status 556 count do not move between the `pause` and `resume` snapshots.
- **b. Bid slots:** CB still logs `auction winner ... transport="stream"` where the stream
  carried the best bid.
- **c. The stream ends with a close frame:** in the capture, Helix sends a WebSocket close
  (opcode 8) on every no-bid stream, and on at least one it arrives before CB's own FIN,
  so CB read it. CB logs its debug `no bid` (the stream ended without error) for every
  no-bid request.
  - Helix's deadline and CB's fall within about 1 ms of each other. When CB's fires first,
    it closes the socket. Helix's close frame then arrives after CB's FIN and draws a RST
    from CB's kernel. That ordering is benign, because CB has already returned.
- **negative:** 7/7 bad upgrades get a 4xx carrying the intended check's message, and no `101`.

Each scenario writes `run/scenario-<utc>/`. It holds the service logs, `cb-metrics-*.txt`,
`cb-helix.pcap` with its tshark export `cb-helix.tsv`, `negative.out`, `images.txt` and
`summary.txt`.

## Extra tools (`tools/`)

- `stream_tail.sh`: adds relay->CB delay (tc netem in a sidecar on helix-relay's egress toward CB
  only), steps it through `DELAYS`, holds each for `SLOTS_PER` slots, verifies each step with ping and
  a one-way UDP probe (`owd.py`), and captures the CB<->relay traffic. `UPLINK=1` adds the same delay
  on CB's egress toward the relay, so the delay is symmetric and CB's TCP connect RTT is 2D.
  `analyze_tail.py <out-dir>` prints frames sent vs received, last-sent vs served bid, the winning
  transport, and the X-Timeout-Ms CB put on the wire next to Helix's window, per slot and per delay.
  Run it against an enclave from `up.sh` with the injector posting rising bids.
- `verify_bids.py`: pulls every SignedExecutionPayloadBid Helix served from the capture and verifies
  its BLS signature against the relay key under the builder domain. CB's ePBS validation does not
  check bid signatures, so a green CB log says nothing about signing.
