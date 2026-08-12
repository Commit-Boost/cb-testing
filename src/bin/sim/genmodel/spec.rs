//! `ScenarioSpec` — the composable, structured surface for CB test scenarios.
//!
//! A scenario is a flat struct of closed enums / `Option`s. It is the surface a
//! caller (a human, or an agent emitting JSON) targets, it is `lower()`'s input,
//! and its `armed_features()` is the verifier oracle. Closed enums make illegal
//! VALUES inexpressible; `lower()` is total on every constructible spec except the
//! one genuinely unrenderable family (mux + any CB-config injection), which it
//! rejects with a clear error rather than a silent wrong config.
//!
//! `lower()` reuses the existing assembly seams verbatim (`CbParams`, `cb_toml`,
//! `cb_toml_mux`, `build_mev_params`, `ElCl`, `poisoned_relay_url`,
//! `load_pubkeys`); it introduces no new YAML/TOML emission. The 13 named
//! scenarios are reproduced byte-for-byte (see `Scenario::to_spec` + the
//! `lower_reproduces_every_scenario` test), so byte-golden acceptance is
//! preserved; the combinatorial space is guarded by offline property tests.

use std::path::Path;

use eyre::Result;

use super::cb::{CbParams, SignerParams, cb_toml, cb_toml_mux};
use super::scenario::{
    COMMON_ADDITIONAL_SERVICES, COMMON_NETWORK_PARAMS, ElCl, Images, MUX_NETWORK_PARAMS,
    build_mev_params, load_pubkeys, poisoned_relay_url,
};
use cb_testnet_verifier::checks::feature_fired::Feature;

/// The api key the ws stream authenticates with — a fixed devnet UUID that rides
/// validator registration so helix TOFU-binds it (see the `cb-ws-stream` comment).
const WS_API_KEY: &str = "9d5c2f4e-1b7a-4c3d-8e6f-0a1b2c3d4e5f";

/// The EL/CL client pair (Law 7: coverage is a matrix, not a point). The CL is
/// the axis that matters for CB behavior (the blinded-block / get_header flow),
/// so the additional pairs vary the CL against geth; `nethermind-prysm` keeps
/// its historical EL pairing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientPair {
    #[default]
    GethLighthouse,
    NethermindPrysm,
    GethTeku,
    GethNimbus,
    GethLodestar,
}

/// Relay topology. Encodes relay count AND the subsidy intent in one knob:
/// `DivergentRelays` is the `[1, 2]` per-relay subsidy split that makes best-bid
/// selection a real discrimination; `TwoRelays` is two relays on the shared `1`
/// subsidy (the timing-games shape); `Mux` is per-node `[[mux]]` routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Topology {
    #[default]
    Single,
    TwoRelays,
    DivergentRelays,
    Mux,
}

/// Whether the ws stream carries its api key. `Absent` is the negative control:
/// helix refuses the handshake, every slot falls back to HTTP, and the ws proof
/// is expected inconclusive (this is `cb-ws-stream-nokey`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyPresence {
    Present,
    Absent,
}

/// getHeader transport. `Stream` sets `get_header = "stream"` per relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeaderTransport {
    #[default]
    Http,
    Stream {
        api_key: KeyPresence,
    },
}

/// Signature-verification mode. Collapses the mutually-exclusive skip/poison
/// combinations into one closed choice so illegal shapes are not constructible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Sigverify {
    #[default]
    On,
    /// `skip_sigverify = true`, clean relay — the codepath exists but a plain run
    /// cannot positively observe it (honest WARN, not a failure).
    Skip,
    /// `skip_sigverify = true` + a wrong-pubkey literal relay: an auction winner
    /// is positive proof the skip codepath fired (the differential treatment arm).
    SkipPoisoned,
    /// Wrong-pubkey literal relay, skip OFF: CB rejects every bid (the control arm,
    /// expected to fail payload delivery).
    PoisonedControl,
}

/// The `min_bid_eth` floor. `Floor` forces the builder subsidy to 0 (a floor is
/// only meaningful with the subsidy off), so subsidy is derived, never a field.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MinBid {
    None,
    Floor(f64),
}

/// The composable scenario surface. Every field is a closed enum / `Option`, so
/// no value outside the modelled space is expressible. `Default` == `cb-basic`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ScenarioSpec {
    pub clients: ClientPair,
    pub topology: Topology,
    pub get_header: HeaderTransport,
    pub timing_games: bool,
    pub extra_validation: bool,
    pub signer: bool,
    pub sigverify: Sigverify,
    pub min_bid: MinBid,
}

impl Default for ScenarioSpec {
    /// The baseline = cb-basic: every knob off. Overlays move deltas off this.
    fn default() -> Self {
        Self {
            clients: ClientPair::default(),
            topology: Topology::default(),
            get_header: HeaderTransport::default(),
            timing_games: false,
            extra_validation: false,
            signer: false,
            sigverify: Sigverify::default(),
            min_bid: MinBid::None,
        }
    }
}

impl ScenarioSpec {
    /// Parse a full `ScenarioSpec` from JSON (the AI-drivable surface). Unknown
    /// keys are rejected (`deny_unknown_fields`); unspecified fields take their
    /// `Default` (= cb-basic), so a partial JSON is a delta off the baseline.
    pub fn from_json(json: &str) -> Result<ScenarioSpec> {
        serde_json::from_str(json).map_err(|err| eyre::eyre!("invalid ScenarioSpec JSON: {err}"))
    }

    /// Resolve a spec from an optional named base plus a comma-separated
    /// `key=value` override string — the deterministic keyword-overlay surface
    /// (`--base cb-mux --set get_header=stream,clients=nethermind-prysm`). No
    /// model: the base is a named scenario's spec, each override is a typed field.
    pub fn from_base_and_overrides(base: Option<&str>, set: Option<&str>) -> Result<ScenarioSpec> {
        let mut spec = match base {
            Some(name) => super::scenario::Scenario::from_name(name)
                .ok_or_else(|| eyre::eyre!("unknown base scenario {name:?}"))?
                .to_spec(),
            None => ScenarioSpec::default(),
        };
        if let Some(sets) = set {
            for pair in sets.split(',').filter(|s| !s.trim().is_empty()) {
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| eyre::eyre!("bad --set entry {pair:?}, want key=value"))?;
                spec.apply_override(k.trim(), v.trim())?;
            }
        }
        Ok(spec)
    }

    /// Apply one `key=value` override. Unknown keys / values are a loud error;
    /// enum values reuse the serde kebab-case names (`nethermind-prysm`, etc.).
    pub fn apply_override(&mut self, key: &str, value: &str) -> Result<()> {
        fn de<T: serde::de::DeserializeOwned>(key: &str, v: &str) -> Result<T> {
            serde_json::from_value(serde_json::Value::String(v.to_string()))
                .map_err(|err| eyre::eyre!("bad value {v:?} for {key}: {err}"))
        }
        fn de_bool(key: &str, v: &str) -> Result<bool> {
            match v {
                "true" => Ok(true),
                "false" => Ok(false),
                other => eyre::bail!("bad bool {other:?} for {key}; want true|false"),
            }
        }
        match key {
            "clients" => self.clients = de(key, value)?,
            "topology" => self.topology = de(key, value)?,
            "sigverify" => self.sigverify = de(key, value)?,
            "timing_games" => self.timing_games = de_bool(key, value)?,
            "extra_validation" => self.extra_validation = de_bool(key, value)?,
            "signer" => self.signer = de_bool(key, value)?,
            "get_header" => {
                self.get_header = match value {
                    "http" => HeaderTransport::Http,
                    "stream" => HeaderTransport::Stream {
                        api_key: KeyPresence::Present,
                    },
                    "stream-nokey" => HeaderTransport::Stream {
                        api_key: KeyPresence::Absent,
                    },
                    other => {
                        eyre::bail!("bad get_header {other:?}; want http|stream|stream-nokey")
                    }
                }
            }
            "min_bid" => {
                self.min_bid = match value {
                    "none" => MinBid::None,
                    f => MinBid::Floor(
                        f.parse()
                            .map_err(|err| eyre::eyre!("bad min_bid floor {f:?}: {err}"))?,
                    ),
                }
            }
            other => eyre::bail!(
                "unknown key {other:?}; want one of clients|topology|get_header|timing_games|\
                 extra_validation|signer|sigverify|min_bid"
            ),
        }
        Ok(())
    }

    /// The EL/CL pair this spec runs on.
    pub fn el_cl(&self) -> ElCl {
        match self.clients {
            ClientPair::GethLighthouse => ElCl::DEFAULT,
            ClientPair::NethermindPrysm => ElCl::ALT,
            ClientPair::GethTeku => ElCl {
                el: "geth",
                cl: "teku",
            },
            ClientPair::GethNimbus => ElCl {
                el: "geth",
                cl: "nimbus",
            },
            ClientPair::GethLodestar => ElCl {
                el: "geth",
                cl: "lodestar",
            },
        }
    }

    /// The relay list. Single = one helix; every multi topology = two helix.
    fn relays(&self) -> &'static [&'static str] {
        match self.topology {
            Topology::Single => &["helix"],
            Topology::TwoRelays | Topology::DivergentRelays | Topology::Mux => &["helix", "helix"],
        }
    }

    /// The builder-subsidy YAML value. Derived from topology + min_bid: a floor
    /// forces 0; divergent relays use the `[1, 2]` split; else the scalar 1.
    fn builder_subsidy(&self) -> &'static str {
        match (self.topology, self.min_bid) {
            (_, MinBid::Floor(_)) => "0",
            (Topology::DivergentRelays, _) => "[1, 2]",
            _ => "1",
        }
    }

    fn network_params(&self) -> &'static str {
        match self.topology {
            Topology::Mux => MUX_NETWORK_PARAMS,
            _ => COMMON_NETWORK_PARAMS,
        }
    }

    /// Compose the `CbParams` seam lines in a FIXED canonical order. The order
    /// is unconstrained by the 13 goldens (none combines two `[pbs]` features),
    /// so a dedicated composite test pins it — a reorder there is the silent
    /// byte-drift risk. Mux does NOT go through here (it has no injection seam).
    fn to_cb_params(&self) -> CbParams {
        let mut p = CbParams::basic();

        // Timeouts ride timing-games.
        if self.timing_games {
            p.timeout_get_header_ms = 400;
            p.timeout_get_payload_ms = 2000;
        }

        // [pbs] lines, canonical order: skip_sigverify, extra_validation, min_bid.
        let mut pbs = Vec::new();
        if matches!(self.sigverify, Sigverify::Skip | Sigverify::SkipPoisoned) {
            pbs.push("skip_sigverify = true".to_string());
        }
        if self.extra_validation {
            pbs.push("extra_validation_enabled = true".to_string());
            pbs.push(format!(r#"rpc_url = "{}""#, self.el_cl().el_rpc_url()));
        }
        if let MinBid::Floor(x) = self.min_bid {
            pbs.push(format!("min_bid_eth = {x}"));
        }
        p.extra_pbs_lines = pbs;

        // Per-relay lines, canonical order: timing-games, then ws stream.
        let mut per_relay = Vec::new();
        if self.timing_games {
            per_relay.extend([
                "enable_timing_games = true".to_string(),
                "target_first_request_ms = 100".to_string(),
                "frequency_get_header_ms = 200".to_string(),
            ]);
        }
        if let HeaderTransport::Stream { api_key } = self.get_header {
            per_relay.push(r#"get_header = "stream""#.to_string());
            if matches!(api_key, KeyPresence::Present) {
                per_relay.push(format!(r#"headers = {{ X-Api-Key = "{WS_API_KEY}" }}"#));
            }
        }
        p.per_relay_lines = per_relay;

        // Fault injection: a wrong-pubkey literal relay replaces the range loop.
        if matches!(
            self.sigverify,
            Sigverify::SkipPoisoned | Sigverify::PoisonedControl
        ) {
            p.literal_relay_url = Some(poisoned_relay_url());
        }

        if self.signer {
            p.signer = Some(SignerParams::devnet());
        }

        p
    }

    /// The `feature_fired::Feature`s this spec arms — a pure projection of the
    /// config knobs onto the verifier's feature enum. This is what the round-trip
    /// test pins against `detect_enabled_features(lower(spec))`. It is 2-valued
    /// (armed / not): whether a feature is PROVEN is a runtime outcome the spec
    /// cannot know, so it is deliberately not modelled here. `min_bid` and the
    /// poison relay live outside the `Feature` enum (separate detectors).
    pub fn armed_features(&self) -> Vec<Feature> {
        let mut f = Vec::new();
        if matches!(self.sigverify, Sigverify::Skip | Sigverify::SkipPoisoned) {
            f.push(Feature::SkipSigverify);
        }
        if self.extra_validation {
            f.push(Feature::ExtraValidation);
        }
        if self.timing_games {
            f.push(Feature::TimingGames);
        }
        if matches!(self.get_header, HeaderTransport::Stream { .. }) {
            f.push(Feature::WsHeaderStream);
        }
        f
    }

    /// True when this spec sets a `min_bid_eth` floor (detected separately from
    /// the `Feature` enum, via `detect_min_bid_eth`).
    pub fn arms_min_bid(&self) -> bool {
        matches!(self.min_bid, MinBid::Floor(_))
    }

    /// True when this spec injects the wrong-pubkey literal relay (detected via
    /// `has_poisoned_relay_pubkey`).
    pub fn arms_poison(&self) -> bool {
        matches!(
            self.sigverify,
            Sigverify::SkipPoisoned | Sigverify::PoisonedControl
        )
    }

    /// Render the full Kurtosis args-file, with `comment` as the leading block.
    ///
    /// The comment is a render-time parameter, NOT a spec field: it is
    /// hand-written per-scenario prose with no knob preimage, so it is not part
    /// of a scenario's structured identity. The 13 named scenarios pass their
    /// verbatim `Scenario::comment()`; composed / AI specs pass `auto_comment()`.
    ///
    /// Total on every constructible spec EXCEPT mux combined with any CB-config
    /// injection: `cb_toml_mux` is a structurally different template with no
    /// `[pbs]`/per-relay/literal-relay seam, so those combinations cannot be
    /// rendered and are rejected loudly rather than silently dropping the
    /// injected config. `keys_dir` is read only for mux (the per-node pubkeys).
    pub fn render(&self, comment: &str, images: &Images, keys_dir: &Path) -> Result<String> {
        let cb_block = if matches!(self.topology, Topology::Mux) {
            eyre::ensure!(
                !self.timing_games
                    && !self.extra_validation
                    && !self.signer
                    && matches!(self.sigverify, Sigverify::On)
                    && matches!(self.min_bid, MinBid::None)
                    && matches!(self.get_header, HeaderTransport::Http),
                "mux uses a fixed CB TOML (per-node [[mux]] routing) with no injection seam; \
                 it cannot compose with pbs/per-relay/literal-relay features"
            );
            let node0 = load_pubkeys(keys_dir, 0)?;
            let node1 = load_pubkeys(keys_dir, 1)?;
            cb_toml_mux(&node0, &node1)
        } else {
            cb_toml(&self.to_cb_params())
        };

        let mev_params = build_mev_params(
            self.relays(),
            images,
            &cb_block,
            self.builder_subsidy(),
            self.signer,
        );

        Ok([
            comment.to_string(),
            self.el_cl().participants(),
            COMMON_ADDITIONAL_SERVICES.to_string(),
            "mev_type: custom".to_string(),
            mev_params,
            self.network_params().to_string(),
        ]
        .join("\n\n")
            + "\n")
    }

    /// A generated comment for a composed / AI-authored spec (no verbatim prose
    /// exists). One `#` header line naming the non-default knobs, so a rendered
    /// config is self-describing without a hand-written block.
    pub fn auto_comment(&self) -> String {
        let mut knobs: Vec<String> = Vec::new();
        if self.clients == ClientPair::NethermindPrysm {
            knobs.push("nethermind-prysm".to_string());
        }
        match self.topology {
            Topology::Single => {}
            Topology::TwoRelays => knobs.push("two-relays".to_string()),
            Topology::DivergentRelays => knobs.push("divergent-relays".to_string()),
            Topology::Mux => knobs.push("mux".to_string()),
        }
        match self.get_header {
            HeaderTransport::Http => {}
            HeaderTransport::Stream {
                api_key: KeyPresence::Present,
            } => knobs.push("ws-stream".to_string()),
            HeaderTransport::Stream {
                api_key: KeyPresence::Absent,
            } => knobs.push("ws-stream-nokey".to_string()),
        }
        if self.timing_games {
            knobs.push("timing-games".to_string());
        }
        if self.extra_validation {
            knobs.push("extra-validation".to_string());
        }
        if self.signer {
            knobs.push("signer".to_string());
        }
        match self.sigverify {
            Sigverify::On => {}
            Sigverify::Skip => knobs.push("skip-sigverify".to_string()),
            Sigverify::SkipPoisoned => knobs.push("skip-sigverify+poison".to_string()),
            Sigverify::PoisonedControl => knobs.push("poison-control".to_string()),
        }
        if let MinBid::Floor(x) = self.min_bid {
            knobs.push(format!("min-bid={x}"));
        }
        let body = if knobs.is_empty() {
            "cb-basic".to_string()
        } else {
            knobs.join(", ")
        };
        format!("# composed scenario: {body}")
    }
}

/// Curated coverage points worth freezing as regression anchors: high-value
/// composed scenarios and the additional CL clients. Each is a `ScenarioSpec`
/// (composed, not a `Scenario` enum variant) with a byte-golden under
/// `tests/fixtures/curated-configs/`, and each has been confirmed to stand up a
/// live devnet (Law 7 / the bench discipline: a golden of a config that has
/// never run is worthless). New entries land WITH a live confirmation.
pub fn curated() -> Vec<(&'static str, ScenarioSpec)> {
    let basic_on = |clients: ClientPair| ScenarioSpec {
        clients,
        ..ScenarioSpec::default()
    };
    vec![
        // The additional CL clients (basic MEV pipeline on each — Law 7).
        ("cb-basic-teku", basic_on(ClientPair::GethTeku)),
        ("cb-basic-nimbus", basic_on(ClientPair::GethNimbus)),
        ("cb-basic-lodestar", basic_on(ClientPair::GethLodestar)),
        // ws stream on the prysm pair — the exact Law-7 route-coupling concern
        // (a prysm-specific ws regression is invisible under geth+lighthouse).
        (
            "cb-ws-prysm",
            ScenarioSpec {
                clients: ClientPair::NethermindPrysm,
                get_header: HeaderTransport::Stream {
                    api_key: KeyPresence::Present,
                },
                ..ScenarioSpec::default()
            },
        ),
        // The composition anchor: two markers must both fire on one run.
        (
            "cb-timing-extra-validation",
            ScenarioSpec {
                topology: Topology::TwoRelays,
                timing_games: true,
                extra_validation: true,
                ..ScenarioSpec::default()
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use cb_testnet_verifier::checks::feature_fired::{
        detect_enabled_features, detect_min_bid_eth, has_poisoned_relay_pubkey,
    };

    use super::*;
    use crate::genmodel::scenario::Scenario;

    /// The keys dir the mux scenario reads (real repo fixture, as the golden
    /// tests use). Non-mux specs ignore it.
    fn keys() -> &'static Path {
        Path::new("keys")
    }

    /// Every `ClientPair` variant, for exhaustive coverage in the offline tests.
    const ALL_CLIENT_PAIRS: [ClientPair; 5] = [
        ClientPair::GethLighthouse,
        ClientPair::NethermindPrysm,
        ClientPair::GethTeku,
        ClientPair::GethNimbus,
        ClientPair::GethLodestar,
    ];

    /// Each client pair renders its own `el_type`/`cl_type` into the participants
    /// block (the parametric axis is real end to end — Law 7). Offline shape check;
    /// standing the client up on a devnet is a separate live confirmation.
    #[test]
    fn every_client_pair_renders_its_el_cl() {
        let images = Images::default();
        for c in ALL_CLIENT_PAIRS {
            let spec = ScenarioSpec {
                clients: c,
                ..ScenarioSpec::default()
            };
            let el = spec.el_cl();
            let out = spec.render("# x", &images, keys()).unwrap();
            assert!(
                out.contains(&format!("el_type: {}", el.el)),
                "missing el_type {} for {c:?}",
                el.el
            );
            assert!(
                out.contains(&format!("cl_type: {}", el.cl)),
                "missing cl_type {} for {c:?}",
                el.cl
            );
        }
    }

    fn sorted_ids(mut fs: Vec<Feature>) -> Vec<&'static str> {
        fs.sort_by_key(|f| f.id());
        fs.dedup();
        fs.iter().map(|f| f.id()).collect()
    }

    /// THE MIGRATION CONTRACT: the composable `render` path reproduces every
    /// named scenario byte-for-byte against the existing (byte-golden'd)
    /// `args_file_in`. If this passes, `render` inherits the goldens' coverage.
    #[test]
    fn lower_reproduces_every_scenario() {
        let images = Images::default();
        for s in Scenario::ALL {
            let via_spec = s.to_spec().render(s.comment(), &images, keys()).unwrap();
            let via_assembly = s.args_file_in(&images, keys()).unwrap();
            assert_eq!(
                via_spec,
                via_assembly,
                "render(spec) != args_file_in for {}",
                s.name()
            );
        }
    }

    /// Round-trip: what the config ARMS (per the spec) equals what the verifier's
    /// own `detect_enabled_features` sees in the rendered config. This is a
    /// RENDERER-DRIFT guard only — it proves emit and detect agree on the toggle
    /// keys; it does NOT prove the config is valid CB (both sides share key
    /// strings, so a shared typo passes — that is `sim preflight`'s job).
    #[test]
    fn armed_features_round_trip_over_the_named_set() {
        let images = Images::default();
        for s in Scenario::ALL {
            let spec = s.to_spec();
            let rendered = spec.render(s.comment(), &images, keys()).unwrap();
            assert_eq!(
                sorted_ids(detect_enabled_features(&rendered)),
                sorted_ids(spec.armed_features()),
                "armed/detected feature mismatch for {}",
                s.name()
            );
            assert_eq!(
                detect_min_bid_eth(&rendered).is_some(),
                spec.arms_min_bid(),
                "min_bid arm/detect mismatch for {}",
                s.name()
            );
            assert_eq!(
                has_poisoned_relay_pubkey(&rendered),
                spec.arms_poison(),
                "poison arm/detect mismatch for {}",
                s.name()
            );
        }
    }

    /// Pin the canonical fragment order for a COMPOSITE spec — the 13 goldens
    /// each populate at most one `[pbs]` feature, so only a composite catches a
    /// canonical-order regression (the silent byte-drift risk the grill flagged).
    #[test]
    fn composite_fragment_order_is_canonical() {
        let images = Images::default();
        // All three [pbs] features + both per-relay features on one spec.
        let spec = ScenarioSpec {
            sigverify: Sigverify::Skip,
            extra_validation: true,
            min_bid: MinBid::Floor(0.5),
            timing_games: true,
            get_header: HeaderTransport::Stream {
                api_key: KeyPresence::Present,
            },
            topology: Topology::TwoRelays,
            ..ScenarioSpec::default()
        };
        let out = spec.render("# composite", &images, keys()).unwrap();
        let at = |needle: &str| {
            out.find(needle)
                .unwrap_or_else(|| panic!("missing {needle}"))
        };
        // [pbs] order: skip_sigverify < extra_validation_enabled < min_bid_eth
        assert!(at("skip_sigverify = true") < at("extra_validation_enabled = true"));
        assert!(at("extra_validation_enabled = true") < at("min_bid_eth = 0.5"));
        // per-relay order: enable_timing_games < get_header = "stream"
        assert!(at("enable_timing_games = true") < at(r#"get_header = "stream""#));
    }

    /// A representative slice of the composable space. Mux is exclusive, so it is
    /// enumerated alone; every other axis combines with the non-mux base.
    fn enumerate() -> Vec<ScenarioSpec> {
        let mut out = Vec::new();
        let clients = ALL_CLIENT_PAIRS;
        let transports = [
            HeaderTransport::Http,
            HeaderTransport::Stream {
                api_key: KeyPresence::Present,
            },
            HeaderTransport::Stream {
                api_key: KeyPresence::Absent,
            },
        ];
        let sigverifies = [
            Sigverify::On,
            Sigverify::Skip,
            Sigverify::SkipPoisoned,
            Sigverify::PoisonedControl,
        ];
        let topos = [
            Topology::Single,
            Topology::TwoRelays,
            Topology::DivergentRelays,
        ];
        for &c in &clients {
            for &t in &transports {
                for &sv in &sigverifies {
                    for &topo in &topos {
                        for tg in [false, true] {
                            for ev in [false, true] {
                                for mb in [MinBid::None, MinBid::Floor(0.5)] {
                                    out.push(ScenarioSpec {
                                        clients: c,
                                        topology: topo,
                                        get_header: t,
                                        timing_games: tg,
                                        extra_validation: ev,
                                        signer: false,
                                        sigverify: sv,
                                        min_bid: mb,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        // Mux alone (exclusive): each client pair.
        for &c in &clients {
            out.push(ScenarioSpec {
                clients: c,
                topology: Topology::Mux,
                ..ScenarioSpec::default()
            });
        }
        out
    }

    /// `render` is total across the non-mux composable space (no panic, always
    /// Ok), and the round-trip holds for every enumerated point — the offline
    /// "enumerable/testable" deliverable (a test, not a coverage-claiming verb).
    #[test]
    fn every_enumerated_spec_renders_and_round_trips() {
        let images = Images::default();
        for spec in enumerate() {
            let rendered = spec
                .render(&spec.auto_comment(), &images, keys())
                .unwrap_or_else(|e| panic!("render failed for {spec:?}: {e}"));
            assert_eq!(
                sorted_ids(detect_enabled_features(&rendered)),
                sorted_ids(spec.armed_features()),
                "round-trip mismatch for {spec:?}"
            );
        }
    }

    #[test]
    fn from_json_is_a_delta_off_the_default_and_rejects_unknown_fields() {
        // Partial JSON: only the named fields move; the rest default to cb-basic.
        let spec = ScenarioSpec::from_json(r#"{"clients":"nethermind-prysm","timing_games":true}"#)
            .unwrap();
        assert_eq!(spec.clients, ClientPair::NethermindPrysm);
        assert!(spec.timing_games);
        assert_eq!(spec.topology, Topology::Single); // defaulted
        // An invented key is a hard error (deny_unknown_fields), not silently dropped.
        assert!(ScenarioSpec::from_json(r#"{"turbo_mode":true}"#).is_err());
    }

    #[test]
    fn from_base_and_overrides_composes_onto_a_named_base() {
        // No base + no set == default (cb-basic).
        assert_eq!(
            ScenarioSpec::from_base_and_overrides(None, None).unwrap(),
            ScenarioSpec::default()
        );
        // A named base resolves to that scenario's spec.
        assert_eq!(
            ScenarioSpec::from_base_and_overrides(Some("cb-mux"), None).unwrap(),
            Scenario::Mux.to_spec()
        );
        // Overrides apply onto the base.
        let spec = ScenarioSpec::from_base_and_overrides(
            Some("cb-basic"),
            Some("get_header=stream,clients=nethermind-prysm,timing_games=true"),
        )
        .unwrap();
        assert_eq!(
            spec.get_header,
            HeaderTransport::Stream {
                api_key: KeyPresence::Present
            }
        );
        assert_eq!(spec.clients, ClientPair::NethermindPrysm);
        assert!(spec.timing_games);
    }

    #[test]
    fn override_errors_are_loud() {
        let mut s = ScenarioSpec::default();
        assert!(s.apply_override("nonsense", "x").is_err()); // unknown key
        assert!(s.apply_override("clients", "solana").is_err()); // bad enum value
        assert!(s.apply_override("timing_games", "yes").is_err()); // bad bool
        assert!(s.apply_override("min_bid", "abc").is_err()); // bad float
        assert!(ScenarioSpec::from_base_and_overrides(Some("no-such-base"), None).is_err());
    }

    /// The curated coverage points render stably against their committed golden.
    /// Regenerate the goldens with `BLESS_CURATED=1 cargo test --bin sim
    /// every_curated_spec_matches_its_golden` (only after a live devnet run
    /// confirms each config actually works).
    #[test]
    fn every_curated_spec_matches_its_golden() {
        let images = Images::default();
        let dir = "tests/fixtures/curated-configs";
        for (name, spec) in curated() {
            let rendered = spec.render(&spec.auto_comment(), &images, keys()).unwrap();
            let path = format!("{dir}/{name}.yml");
            if std::env::var("BLESS_CURATED").is_ok() {
                std::fs::create_dir_all(dir).unwrap();
                std::fs::write(&path, &rendered).unwrap();
            } else {
                let golden = std::fs::read_to_string(&path).unwrap_or_else(|_| {
                    panic!("missing curated golden {path}; run BLESS_CURATED=1 to create it")
                });
                assert_eq!(
                    rendered, golden,
                    "curated config {name} drifted from its golden"
                );
            }
        }
    }

    /// Mux composed with any injection feature is rejected loudly (not a silent
    /// wrong config) — the one genuinely unrenderable family.
    #[test]
    fn mux_with_injection_is_rejected() {
        let images = Images::default();
        let bad = ScenarioSpec {
            topology: Topology::Mux,
            timing_games: true,
            ..ScenarioSpec::default()
        };
        assert!(bad.render("# x", &images, keys()).is_err());
    }
}
