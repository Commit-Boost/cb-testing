#!/usr/bin/env python3
"""
cb-verify: Automated verification for Commit-Boost Kurtosis testnets.

Discovers services in a running enclave, polls for readiness,
runs verification checks, and produces a structured report.

Usage:
    python3 -m cb_verifier --enclave CB-Testnet [--min-epochs 2] [--timeout 1500] [--json]
"""

import argparse
import logging
import sys
import time
from datetime import datetime, timezone

import requests

from cb_verifier.discovery import discover, EnclaveServices
from cb_verifier.report import (
    CheckResult, ResultStatus, VerificationReport,
    print_report, exit_code,
)
from cb_verifier.checks.chain_health import run_chain_health_checks
from cb_verifier.checks.relay_pipeline import run_relay_checks
from cb_verifier.checks.payload_matching import run_payload_checks
from cb_verifier.checks.cb_metrics import run_metrics_checks

logger = logging.getLogger("cb-verify")

SLOTS_PER_EPOCH = 32


# ---------------------------------------------------------------------------
# Beacon API helpers
# ---------------------------------------------------------------------------

def get_head_slot(beacon_url: str) -> int | None:
    """Get the current head slot from the beacon node."""
    try:
        resp = requests.get(
            f"{beacon_url}/eth/v1/beacon/headers/head", timeout=10
        )
        resp.raise_for_status()
        return int(resp.json()["data"]["header"]["message"]["slot"])
    except Exception as e:
        logger.warning("Failed to get head slot: %s", e)
        return None


def get_finalized_epoch(beacon_url: str) -> int | None:
    """Get the finalized epoch from the beacon node."""
    try:
        resp = requests.get(
            f"{beacon_url}/eth/v1/beacon/states/head/finality_checkpoints",
            timeout=10,
        )
        resp.raise_for_status()
        return int(resp.json()["data"]["finalized"]["epoch"])
    except Exception as e:
        logger.warning("Failed to get finalized epoch: %s", e)
        return None


def is_syncing(beacon_url: str) -> bool | None:
    """Check if the beacon node is syncing. Returns None on error."""
    try:
        resp = requests.get(
            f"{beacon_url}/eth/v1/node/syncing", timeout=10
        )
        resp.raise_for_status()
        return resp.json()["data"]["is_syncing"]
    except Exception as e:
        logger.warning("Failed to check sync status: %s", e)
        return None


def get_genesis_time(beacon_url: str) -> int | None:
    """Get genesis time from the beacon node."""
    try:
        resp = requests.get(
            f"{beacon_url}/eth/v1/beacon/genesis", timeout=10
        )
        resp.raise_for_status()
        return int(resp.json()["data"]["genesis_time"])
    except Exception as e:
        logger.warning("Failed to get genesis time: %s", e)
        return None


def get_seconds_per_slot(beacon_url: str) -> int:
    """Get seconds_per_slot from the config. Defaults to 12."""
    try:
        resp = requests.get(
            f"{beacon_url}/eth/v1/config/spec", timeout=10
        )
        resp.raise_for_status()
        return int(resp.json()["data"].get("SECONDS_PER_SLOT", 12))
    except Exception:
        return 12


# ---------------------------------------------------------------------------
# Readiness polling
# ---------------------------------------------------------------------------

def wait_for_readiness(
    beacon_url: str,
    target_epoch: int = 7,
    timeout: int = 1500,
    poll_interval: int = 10,
) -> bool:
    """
    Poll the beacon node until the devnet is ready for verification.

    Ready means:
    - Not syncing
    - Finalized epoch >= 2
    - Head slot is in or past target_epoch

    Returns True when ready, False on timeout.
    """
    start = time.time()
    sps = get_seconds_per_slot(beacon_url)
    logger.info(
        "Waiting for readiness (target epoch %d, slot %d, timeout %ds, %ds slots)...",
        target_epoch, target_epoch * SLOTS_PER_EPOCH, timeout, sps,
    )

    while time.time() - start < timeout:
        syncing = is_syncing(beacon_url)
        if syncing is None:
            logger.info("  Beacon node not reachable yet...")
            time.sleep(poll_interval)
            continue

        if syncing:
            logger.info("  Beacon node still syncing...")
            time.sleep(poll_interval)
            continue

        head = get_head_slot(beacon_url)
        finalized = get_finalized_epoch(beacon_url)
        current_epoch = head // SLOTS_PER_EPOCH if head else 0

        logger.info(
            "  head_slot=%s epoch=%d finalized_epoch=%s (target: epoch>=%d, finalized>=2)",
            head, current_epoch, finalized, target_epoch,
        )

        if (
            head is not None
            and finalized is not None
            and finalized >= 2
            and current_epoch >= target_epoch
        ):
            logger.info("Devnet is ready.")
            return True

        time.sleep(poll_interval)

    logger.error("Timeout: devnet did not stabilize within %ds", timeout)
    return False


# ---------------------------------------------------------------------------
# Observation window
# ---------------------------------------------------------------------------

def observe_epochs(
    beacon_url: str,
    num_epochs: int = 2,
    poll_interval: int = 5,
    timeout: int = 300,
) -> tuple[int, int] | None:
    """
    Wait for num_epochs to pass and return (start_slot, end_slot) of the
    observation window.

    Returns None on timeout.
    """
    start_slot = get_head_slot(beacon_url)
    if start_slot is None:
        return None

    target_slot = start_slot + (num_epochs * SLOTS_PER_EPOCH)
    logger.info(
        "Observing %d epochs: slot %d -> %d",
        num_epochs, start_slot, target_slot,
    )

    start_time = time.time()
    while time.time() - start_time < timeout:
        head = get_head_slot(beacon_url)
        if head is not None and head >= target_slot:
            logger.info("Observation window complete: slot %d -> %d", start_slot, head)
            return (start_slot, head)
        time.sleep(poll_interval)

    logger.error("Timeout waiting for observation window")
    return None


# ---------------------------------------------------------------------------
# Main verification flow
# ---------------------------------------------------------------------------

def run_verification(
    enclave: str,
    min_epochs: int = 2,
    target_epoch: int = 7,
    timeout: int = 1500,
    json_mode: bool = False,
    mev_threshold: float = 0.30,
) -> int:
    """
    Full verification pipeline:
    1. Discover services
    2. Wait for readiness
    3. Observe for min_epochs
    4. Run all checks
    5. Report results

    Returns exit code (0=pass, 1=fail, 2=setup error).
    """

    # -- Step 1: Discover services --
    logger.info("Discovering services in enclave '%s'...", enclave)
    try:
        services = discover(enclave)
    except Exception as e:
        logger.error("Service discovery failed: %s", e)
        report = VerificationReport(
            enclave=enclave,
            timestamp=datetime.now(timezone.utc).isoformat(),
            observation_window={},
            result=ResultStatus.FAIL,
            checks=[CheckResult(
                id="service_discovery",
                tier=1,
                result=ResultStatus.FAIL,
                detail=f"Discovery failed: {e}",
            )],
        )
        print_report(report, json_mode)
        return 2

    if not services.beacon_urls:
        logger.error("No beacon nodes found in enclave")
        report = VerificationReport(
            enclave=enclave,
            timestamp=datetime.now(timezone.utc).isoformat(),
            observation_window={},
            result=ResultStatus.FAIL,
            checks=[CheckResult(
                id="service_discovery",
                tier=1,
                result=ResultStatus.FAIL,
                detail="No beacon nodes found",
            )],
        )
        print_report(report, json_mode)
        return 2

    beacon_url = services.beacon_urls[0]
    relay_urls = services.relay_urls
    metrics_url = services.cb_metrics_urls[0] if services.cb_metrics_urls else None

    logger.info("Beacon: %s", beacon_url)
    logger.info("Relays: %s", relay_urls)
    logger.info("CB metrics: %s", metrics_url or "not available")

    if not relay_urls:
        logger.warning("No relay URLs found -- relay checks will fail")

    # -- Step 2: Wait for readiness --
    if not wait_for_readiness(beacon_url, target_epoch=target_epoch, timeout=timeout):
        report = VerificationReport(
            enclave=enclave,
            timestamp=datetime.now(timezone.utc).isoformat(),
            observation_window={},
            result=ResultStatus.FAIL,
            checks=[CheckResult(
                id="readiness",
                tier=1,
                result=ResultStatus.FAIL,
                detail=f"Devnet did not stabilize within {timeout}s",
            )],
        )
        print_report(report, json_mode)
        return 2

    # -- Step 3: Observe for min_epochs --
    obs_timeout = min_epochs * SLOTS_PER_EPOCH * get_seconds_per_slot(beacon_url) + 120
    window = observe_epochs(
        beacon_url, num_epochs=min_epochs, timeout=obs_timeout
    )
    if window is None:
        report = VerificationReport(
            enclave=enclave,
            timestamp=datetime.now(timezone.utc).isoformat(),
            observation_window={},
            result=ResultStatus.FAIL,
            checks=[CheckResult(
                id="observation_window",
                tier=1,
                result=ResultStatus.FAIL,
                detail="Failed to complete observation window",
            )],
        )
        print_report(report, json_mode)
        return 2

    start_slot, end_slot = window

    # -- Step 4: Run checks --
    all_checks: list[CheckResult] = []

    logger.info("Running chain health checks...")
    all_checks.extend(
        run_chain_health_checks(beacon_url, start_slot, end_slot, enclave)
    )

    logger.info("Running relay pipeline checks...")
    all_checks.extend(
        run_relay_checks(relay_urls, beacon_url, start_slot, end_slot, mev_threshold=mev_threshold)
    )

    logger.info("Running payload matching checks...")
    all_checks.extend(
        run_payload_checks(relay_urls, beacon_url, start_slot, end_slot)
    )

    logger.info("Running CB metrics checks...")
    all_checks.extend(run_metrics_checks(
        metrics_url,
        enclave=enclave,
        cb_services=services.cb_service_names or None,
    ))

    # -- Step 5: Report --
    tier1_failed = any(
        c.result == ResultStatus.FAIL and c.tier == 1
        for c in all_checks
    )

    report = VerificationReport(
        enclave=enclave,
        timestamp=datetime.now(timezone.utc).isoformat(),
        observation_window={"start_slot": start_slot, "end_slot": end_slot},
        result=ResultStatus.FAIL if tier1_failed else ResultStatus.PASS,
        checks=all_checks,
    )

    print_report(report, json_mode)
    return exit_code(report)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="Verify Commit-Boost MEV pipeline in a Kurtosis devnet",
    )
    parser.add_argument(
        "--enclave", required=True,
        help="Kurtosis enclave name",
    )
    parser.add_argument(
        "--min-epochs", type=int, default=2,
        help="Observation window in epochs (default: 2)",
    )
    parser.add_argument(
        "--target-epoch", type=int, default=7,
        help="Wait until this epoch before starting checks (default: 5)",
    )
    parser.add_argument(
        "--timeout", type=int, default=1500,
        help="Max seconds to wait for devnet readiness (default: 1500)",
    )
    parser.add_argument(
        "--mev-threshold", type=float, default=0.30,
        help="Minimum MEV delivery rate (default: 0.30)",
    )
    parser.add_argument(
        "--json", action="store_true", dest="json_mode",
        help="Output JSON report instead of terminal colors",
    )
    parser.add_argument(
        "-v", "--verbose", action="store_true",
        help="Enable debug logging",
    )

    args = parser.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s [%(levelname)s] %(message)s",
        datefmt="%H:%M:%S",
    )

    sys.exit(run_verification(
        enclave=args.enclave,
        min_epochs=args.min_epochs,
        target_epoch=args.target_epoch,
        timeout=args.timeout,
        json_mode=args.json_mode,
        mev_threshold=args.mev_threshold,
    ))


if __name__ == "__main__":
    main()
