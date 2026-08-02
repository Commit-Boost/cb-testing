//! cb-testnet-verifier: shared library.
//!
//! The mature modules that discover services, probe beacon/relay endpoints,
//! run verification checks, and build structured reports live here so every
//! binary in the package (`cb-verify`, `cb-orchestrator`, `sim`, …) can reuse
//! them by import instead of re-declaring or re-implementing them.

pub mod beacon;
pub mod checks;
pub mod discovery;
pub mod metrics;
pub mod relay;
pub mod report;
