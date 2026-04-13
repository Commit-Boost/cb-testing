"""Beacon chain health verification checks."""

import subprocess
import requests
from cb_verifier.report import CheckResult, ResultStatus as CheckStatus


def check_finality(beacon_url: str) -> CheckResult:
    """Check if the beacon chain has finalized past epoch 2."""
    try:
        resp = requests.get(
            f"{beacon_url}/eth/v1/beacon/states/head/finality_checkpoints",
            timeout=10,
        )
        resp.raise_for_status()
        data = resp.json()
        finalized_epoch = int(data["data"]["finalized"]["epoch"])
        if finalized_epoch >= 2:
            return CheckResult(
                id="chain_finality",
                tier=1,
                result=CheckStatus.PASS,
                detail=f"Finalized epoch: {finalized_epoch}",
                data={"finalized_epoch": finalized_epoch},
            )
        else:
            return CheckResult(
                id="chain_finality",
                tier=1,
                result=CheckStatus.FAIL,
                detail=f"Finalized epoch too low: {finalized_epoch} (need >= 2)",
                data={"finalized_epoch": finalized_epoch},
            )
    except Exception as e:
        return CheckResult(
            id="chain_finality",
            tier=1,
            result=CheckStatus.FAIL,
            detail=f"Error checking finality: {e}",
            data={"error": str(e)},
        )


def check_missed_slots(
    beacon_url: str,
    start_slot: int,
    end_slot: int,
    threshold: float = 0.10,
) -> CheckResult:
    """Check missed slot rate over a range of slots."""
    try:
        missed = 0
        total = end_slot - start_slot
        if total <= 0:
            return CheckResult(
                id="missed_slots",
                tier=2,
                result=CheckStatus.FAIL,
                detail="Invalid slot range: start_slot must be less than end_slot",
                data={"start_slot": start_slot, "end_slot": end_slot},
            )
        for slot in range(start_slot, end_slot):
            try:
                resp = requests.get(
                    f"{beacon_url}/eth/v1/beacon/headers/{slot}",
                    timeout=10,
                )
                if resp.status_code == 404:
                    missed += 1
            except requests.RequestException:
                missed += 1

        rate = missed / total
        result_data = {
            "missed": missed,
            "total": total,
            "rate": round(rate, 4),
            "threshold": threshold,
            "start_slot": start_slot,
            "end_slot": end_slot,
        }
        if rate < threshold:
            return CheckResult(
                id="missed_slots",
                tier=2,
                result=CheckStatus.PASS,
                detail=f"Missed {missed}/{total} slots ({rate:.2%}), under {threshold:.0%} threshold",
                data=result_data,
            )
        else:
            return CheckResult(
                id="missed_slots",
                tier=2,
                result=CheckStatus.WARN,
                detail=f"Missed {missed}/{total} slots ({rate:.2%}), above {threshold:.0%} threshold",
                data=result_data,
            )
    except Exception as e:
        return CheckResult(
            id="missed_slots",
            tier=2,
            result=CheckStatus.FAIL,
            detail=f"Error checking missed slots: {e}",
            data={"error": str(e)},
        )


def check_sync_status(beacon_url: str) -> CheckResult:
    """Check if the beacon node is done syncing."""
    try:
        resp = requests.get(
            f"{beacon_url}/eth/v1/node/syncing",
            timeout=10,
        )
        resp.raise_for_status()
        data = resp.json()
        is_syncing = data["data"]["is_syncing"]
        if not is_syncing:
            return CheckResult(
                id="sync_status",
                tier=1,
                result=CheckStatus.PASS,
                detail="Node is fully synced",
                data=data["data"],
            )
        else:
            return CheckResult(
                id="sync_status",
                tier=1,
                result=CheckStatus.FAIL,
                detail="Node is still syncing",
                data=data["data"],
            )
    except Exception as e:
        return CheckResult(
            id="sync_status",
            tier=1,
            result=CheckStatus.FAIL,
            detail=f"Error checking sync status: {e}",
            data={"error": str(e)},
        )


def check_cb_running(
    enclave: str, service_pattern: str = "commit-boost"
) -> CheckResult:
    """Check if commit-boost service is running in the Kurtosis enclave.

    Uses 'kurtosis enclave inspect' and greps for services matching the
    pattern, since CB service names are dynamic (e.g. commit-boost-001-lighthouse-geth).
    """
    try:
        proc = subprocess.run(
            ["kurtosis", "enclave", "inspect", enclave],
            capture_output=True,
            text=True,
            timeout=30,
        )
        if proc.returncode != 0:
            detail = proc.stderr.strip() or "Failed to inspect enclave"
            return CheckResult(
                id="cb_running",
                tier=1,
                result=CheckStatus.FAIL,
                detail=f"Cannot inspect enclave '{enclave}': {detail}",
                data={"enclave": enclave, "returncode": proc.returncode},
            )

        # Find lines matching the service pattern with RUNNING status
        cb_services = []
        for line in proc.stdout.splitlines():
            if service_pattern in line.lower():
                cb_services.append(line.strip())

        running = [s for s in cb_services if "running" in s.lower()]

        if running:
            return CheckResult(
                id="cb_running",
                tier=1,
                result=CheckStatus.PASS,
                detail=f"Found {len(running)} commit-boost service(s) running",
                data={"enclave": enclave, "services": running},
            )
        elif cb_services:
            return CheckResult(
                id="cb_running",
                tier=1,
                result=CheckStatus.FAIL,
                detail=f"Found {len(cb_services)} commit-boost service(s) but none running",
                data={"enclave": enclave, "services": cb_services},
            )
        else:
            return CheckResult(
                id="cb_running",
                tier=1,
                result=CheckStatus.FAIL,
                detail=f"No commit-boost services found in enclave '{enclave}'",
                data={"enclave": enclave},
            )
    except FileNotFoundError:
        return CheckResult(
            id="cb_running",
            tier=1,
            result=CheckStatus.FAIL,
            detail="kurtosis CLI not found on PATH",
            data={"error": "kurtosis not found"},
        )
    except Exception as e:
        return CheckResult(
            id="cb_running",
            tier=1,
            result=CheckStatus.FAIL,
            detail=f"Error inspecting service: {e}",
            data={"error": str(e)},
        )


def run_chain_health_checks(
    beacon_url: str,
    start_slot: int,
    end_slot: int,
    enclave: str,
) -> list[CheckResult]:
    """Run all chain health checks and return results."""
    return [
        check_finality(beacon_url),
        check_missed_slots(beacon_url, start_slot, end_slot),
        check_sync_status(beacon_url),
        check_cb_running(enclave),
    ]
