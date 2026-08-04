---
status: live
---

# Signer module on Kurtosis (North Star)

**Why this is a North Star:** CB's signer module has never been testable on Kurtosis - the
ethereum-package lacked the config support. We control the fork, so it is buildable. This doc is the
researched runtime contract; build against it, do not re-derive it.

**Priority:** dessert. The MEV scenario matrix (docs/SWEEP-BACKLOG.md) comes first - a signer test
riding on an unreliable MEV harness proves nothing.

## The runtime contract (researched from commit-boost-client source, 2026-08-04)

One binary, three subcommands (`bin/commit-boost.rs:27-45`): `pbs`, `signer`, `init`. The published
image ships the same binary with `CMD ["pbs"]`, so a signer container is **the same image with the
command overridden to `["signer"]`** (which is exactly what CB's own compose generator does,
`crates/cli/src/docker_init.rs:463`). Runs as uid/gid **10001** - every mounted file must be readable
by it.

### Minimum container
```
image: commit-boost/commit-boost:kurtosis   (the image we already build)
cmd:   ["signer"]                            # REQUIRED override
user:  10001:10001
port:  20000/tcp                             # + metrics port only if CB_METRICS_PORT is set
```

### Required env (all four, or it will not start)
```
CB_CONFIG=/cb-config.toml            # no runtime fallback despite the CONFIG_DEFAULT constant
CB_JWTS=TEST_MODULE=<secret>         # module-id -> shared secret map
CB_SIGNER_ADMIN_JWT=<secret>         # required even if admin routes are never called
CB_SIGNER_ENDPOINT=0.0.0.0:20000     # THE DEVNET TRAP: [signer].host defaults to 127.0.0.1,
                                     # which is unreachable cross-container
CB_SIGNER_LOADER_FILE=/keys.json     # File loader (simplest), or the KEYS_DIR/SECRETS_DIR pair
```

### Minimum config TOML
```toml
chain = { genesis_time_secs = <ts>, path = "/chain_spec.json" }
relays = []          # emit EXPLICITLY: the env-load path (HelperConfig) has no serde default
[pbs]                # required table, may be empty
[signer]
port = 20000
[signer.local.loader]
key_path = "/keys.json"
[[modules]]          # REQUIRED - see failure mode 1
id = "TEST_MODULE"
type = "commit"
signing_id = "0x..."     # required, non-zero, unique per module
docker_image = "unused"  # required field, never read by the signer process
```

### Four failure modes that would bite a naive launcher
1. **No `[[modules]]` => the process exits 0, silently.** `SigningService::run` warns "Signing service
   was started but no module is registered. Exiting" and returns `Ok(())` (`service.rs:92-96`).
   Kurtosis sees a clean shutdown, not a crash. **Assert on the log line, not just liveness.**
2. **`relays = []` must be explicit** on the env-load path (`config/mod.rs:175-186`).
3. **`[pbs]` table must exist** even if empty.
4. **Key loading fails SILENTLY**: every directory loader is `filter_map` + `warn!`, so a bad keystore
   is skipped and the service starts with `loaded_consensus=0` (`loader.rs:141-147`). **Assert the
   count, not liveness.**

### Keys: reuse what the devnet already generates
The File loader wants a JSON array of hex secret keys (simplest). The ValidatorsDir loader supports
lighthouse/teku/lodestar/prysm/nimbus keystore layouts - and the ethereum-package already generates
lighthouse-format keystores (`keys/<0xpubkey>/voting-keystore.json` + `secrets/<0xpubkey>` whose
CONTENTS are the raw password). If that artifact can be mounted, the signer needs no new key material.

### What a test asserts (ladder)
1. **Liveness**: `GET /status` => 200, no auth (deliberately unauthenticated, bypasses both auth
   layers; it is what CB's own healthcheck uses).
2. **Keys really loaded**: the startup `info!` line carries `loaded_consensus=N` - assert `== expected`,
   and assert ABSENCE of the "no module is registered. Exiting" line.
3. **End to end**: mint a module JWT (HS256, claims `{module, route, exp, payload_hash}`, route must
   equal the exact request path, `payload_hash` = keccak256(body) or null when there is no body) and
   call `GET /signer/v1/get_pubkeys` => 200 with the expected key count, then
   `POST /signer/v1/request_signature/bls`. BLS signing is deterministic, so with fixed keys +
   `signing_id` + chain id the signature can be pinned byte-exact.
4. **Negative controls** (cheap, high signal): wrong secret => 401; >3 failures from one IP => 429.

### Existing CB tests are NOT a template for the plumbing
`tests/tests/signer_*.rs` start the service in-process via `run_with_listener` with a hand-built
config - they never touch env vars, TOML, Docker, or the CLI. Reuse their ASSERTIONS; the startup
path is untested territory that our launcher would be the first to exercise.

## Open questions before building
- Which ethereum-package artifact holds the validator keystores, and can it be mounted read-only into
  a second container without disturbing the VC? (research pending)
- Do we add a `mev_params.signer_*` knob, or a separate top-level participant-style config?
- Is a commit MODULE container in scope for v1, or is signer-up + keys-loaded + JWT-authed
  get_pubkeys enough to call the North Star reached?
