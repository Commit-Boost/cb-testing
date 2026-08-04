---
status: live
---

> **BUILT 2026-08-04, NOT YET VALIDATED LIVE.** All three pieces are in:
> config-gen (`CbParams.signer` + the `cb-signer` scenario), the fork launcher
> (`ethereum-package/src/mev/commit-boost/signer/signer_launcher.star`, launched
> from `main.star`), and the assertion (`src/checks/signer.rs`, wired into the
> run pipeline). Built to the GRILL VERDICT below, not to the original sketch.
> The remaining work is a devnet run of `cb-signer`; until then nothing here is
> proven to work end to end.

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

## GRILL VERDICT (2026-08-04, independent adversarial review) — BUILD MODIFIED

The review KILLED two choices in this doc and corrected one research error. Build with these five
changes; do not build the version above.

**1. KILL - the key layout silently loads ZERO keys.** `secrets/` is created with `chmod 0600 -R`
(`validator_keystore_generator.star:113-118`), which strips the execute bit from the DIRECTORY, so
uid 10001 cannot traverse it. CB's lighthouse loader is `filter_map` + `warn!`, so this presents as
`loaded_consensus=0` with a healthy process and a 200 on `/status`. Proof it is real: the package
forces the nimbus VC to `User(uid=0, gid=0)` (`src/vc/nimbus.star:142`) SPECIFICALLY because it reads
that 0600 dir, and six other launchers do the same. **Use `format = "teku"` over `teku-keys` +
`teku-secrets`** - `teku-secrets` is never chmodded and web3signer (non-root) reads it today, which is
what actually keeps that launcher alive (not the 0777 on teku-keys). Preflight with one
`kurtosis service shell <enclave> vc-1-... && ls -la /validator-keys/node-0-keystores/` before writing
any starlark.

**1b. EMPIRICALLY CONFIRMED on a live enclave (2026-08-04), before writing any starlark.**
`kurtosis service exec <enclave> vc-1-geth-lighthouse "stat -c '%n %a %U:%G' ..."`:
```
/validator-keys/secrets       600  root:root   drw-------
/validator-keys/teku-secrets  755  root:root   drwxr-xr-x
/validator-keys/teku-keys     777  root:root   drwxrwxrwx
```
`secrets/` has NO execute bit and is root-owned, so uid 10001 cannot even traverse it - the signer
would have started healthy and loaded ZERO keys. Kurtosis does NOT chown on mount (everything is
`root:root` inside the container), which also confirms why six other launchers force `User(uid=0)`.
**`teku-keys` + `teku-secrets` are both readable by a non-root uid. Use them.**

**2. KILL - Option A (participant_network.star) is a dead end.** The CB config artifact does not exist
yet at that point; it is rendered downstream inside `commit_boost_mev_boost.launch`
(`main.star:537`). Option A would force a second, divergent TOML - the exact surgery we were avoiding.
**Build at Option B**: thread `node_keystore_files` through `new_participant` (~5 lines) so
`main.star`'s MEV loop has the keystores, the genesis artifact, the timestamp and the rendered config
in one scope.

**3. `pbs.with_signer` IS DEAD CODE in the shipped binary.** It is read only in
`load_pbs_custom_config` (`config/pbs.rs:424`), which the `pbs` subcommand never calls
(`bin/commit-boost.rs:67` calls `load_pbs_config`). CB's own `config.example.toml:16` says "(not used
in the default PBS image)". So there is NO ordering constraint and no `CB_SIGNER_URL` to thread - and
"PBS uses the signer" is not an available escalation. The only in-CB end-to-end path is a separate
commit-module container; not v1.

**4. CORRECTION to failure mode 1 above:** a MISSING `[[modules]]` bails LOUDLY
(`config/signer.rs:378`, `.ok_or_eyre("No modules defined in the config")`). The silent `Ok(())` exit
is only reachable with an explicit `modules = []`. Mitigation is not a log assertion but a port spec
with `wait="15s"` (as `mev_boost_launcher.star:11-22` already does), which turns the silent-exit class
into a loud `kurtosis run` failure since the emptiness check precedes the listener bind.

**5. The ladder was mostly vanity. Collapse it to three rungs:**
- port `wait="15s"` at launch (catches the silent-exit class)
- **JWT-authed `GET /signer/v1/get_pubkeys` with a COUNT assertion** - this is the smallest honest
  test: it subsumes liveness, module registration, JWT auth AND key loading in one HTTP call, and it
  is by construction the assertion that the permissions bug in (1) would fail.
- one `request_signature/bls` determinism differential: 96 bytes, byte-identical across two identical
  requests (BLS is deterministic), and DIFFERENT for a different `signing_id`. A real differential
  without needing a BLS verifier.

Dropped: `GET /status` (an unconditional `Ok(StatusCode::OK)` with zero logic - and the metrics server
exposes a SECOND unconditional /status, so scraping the wrong port is an even emptier green); and the
`loaded_consensus` log grep (log-only - `crates/signer/src/metrics.rs` registers exactly one metric,
`signer_status_code_total`, with no key-count gauge - AND ANSI-colored by default, so the field is not
a contiguous substring; strictly weaker than the HTTP count).

**Two traps that would burn an afternoon:**
- **Rate-limit self-poisoning:** three wrong-secret probes trip `jwt_auth_fail_limit=3` and 429 the
  harness's own NAT source IP for `jwt_auth_fail_timeout_seconds=300`. Run negative controls LAST and
  set `CB_SIGNER_JWT_AUTH_FAIL_TIMEOUT_SECONDS=5`.
- **Service naming:** `src/discovery.rs` sweeps `commit-boost-*` into `cb_service_names`, and three
  checks then shell `kurtosis service logs -n 200000` per name. Name it **`cb-signer-*`** and give it
  explicit discovery, or every check pays an extra full log fetch.

Also confirmed cheap: one TOML/one artifact works (the signer mounts the same per-participant config
artifact); per-participant key paths ride `CB_SIGNER_LOADER_KEYS_DIR`/`_SECRETS_DIR` env vars rather
than template vars (the template only has .Network/.Port/.Relays/.Timestamp); `[[modules]].docker_image`
causes no Kurtosis pull; the chain spec is already mounted at `/network-configs`. Gate the new TOML
blocks behind an opt-in `CbParams` field so the other eight golden fixtures stay untouched.

## Open questions before building
- Which ethereum-package artifact holds the validator keystores, and can it be mounted read-only into
  a second container without disturbing the VC? (research pending)
- Do we add a `mev_params.signer_*` knob, or a separate top-level participant-style config?
- Is a commit MODULE container in scope for v1, or is signer-up + keys-loaded + JWT-authed
  get_pubkeys enough to call the North Star reached?
