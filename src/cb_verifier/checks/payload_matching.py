"""Cross-references relay delivered payloads with on-chain beacon blocks."""

import logging

import requests

from cb_verifier.report import CheckResult, ResultStatus

logger = logging.getLogger(__name__)

REQUEST_TIMEOUT = 10


def get_delivered_payloads(relay_urls: list[str], start_slot: int, end_slot: int) -> list[dict]:
    """Query relays for delivered payloads, filter by slot range, deduplicate by slot."""
    seen_slots: dict[int, dict] = {}

    for url in relay_urls:
        try:
            resp = requests.get(
                f"{url.rstrip('/')}/relay/v1/data/bidtraces/proposer_payload_delivered",
                params={"cursor": end_slot, "limit": end_slot - start_slot + 1},
                timeout=REQUEST_TIMEOUT,
            )
            resp.raise_for_status()
            entries = resp.json()
        except Exception as e:
            logger.warning("Failed to fetch delivered payloads from %s: %s", url, e)
            continue

        for entry in entries:
            try:
                slot = int(entry.get("slot", 0))
            except (ValueError, TypeError):
                continue

            if slot < start_slot or slot > end_slot:
                continue

            if slot not in seen_slots:
                seen_slots[slot] = {
                    "slot": slot,
                    "block_hash": entry.get("block_hash", ""),
                    "proposer_pubkey": entry.get("proposer_pubkey", ""),
                    "value": entry.get("value", "0"),
                }

    return sorted(seen_slots.values(), key=lambda p: p["slot"])


def get_beacon_block_hash(beacon_url: str, slot: int) -> str | None:
    """Fetch the execution payload block_hash for a given slot from the beacon node."""
    try:
        resp = requests.get(
            f"{beacon_url.rstrip('/')}/eth/v2/beacon/blocks/{slot}",
            timeout=REQUEST_TIMEOUT,
        )
        if resp.status_code == 404:
            return None
        resp.raise_for_status()
        data = resp.json()
        return data["data"]["message"]["body"]["execution_payload"]["block_hash"]
    except Exception as e:
        logger.warning("Failed to get beacon block for slot %d: %s", slot, e)
        return None


def check_payload_hash_match(
    relay_urls: list[str], beacon_url: str, start_slot: int, end_slot: int
) -> CheckResult:
    """Compare relay delivered payload hashes against on-chain beacon block hashes."""
    try:
        payloads = get_delivered_payloads(relay_urls, start_slot, end_slot)
    except Exception as e:
        return CheckResult(
            id="payload_hash_match",
            tier=1,
            result=ResultStatus.FAIL,
            detail=f"Error fetching delivered payloads: {e}",
            data={},
        )

    matched = 0
    mismatched = 0
    missed = 0
    mismatches: list[dict] = []

    for payload in payloads:
        slot = payload["slot"]
        relay_hash = payload["block_hash"]

        chain_hash = get_beacon_block_hash(beacon_url, slot)

        if chain_hash is None:
            missed += 1
            continue

        if relay_hash == chain_hash:
            matched += 1
        else:
            mismatched += 1
            mismatches.append({
                "slot": slot,
                "relay_hash": relay_hash,
                "chain_hash": chain_hash,
            })
            logger.warning(
                "Hash mismatch at slot %d: relay=%s chain=%s (possible reorg)",
                slot, relay_hash, chain_hash,
            )

    result = ResultStatus.FAIL if mismatched > 0 else ResultStatus.PASS
    detail_parts = [f"{matched} matched", f"{mismatched} mismatched", f"{missed} missed"]
    detail = f"Payload hash check: {', '.join(detail_parts)} out of {len(payloads)} delivered payloads"

    return CheckResult(
        id="payload_hash_match",
        tier=1,
        result=result,
        detail=detail,
        data={
            "matched": matched,
            "mismatched": mismatched,
            "missed": missed,
            "mismatches": mismatches,
        },
    )


def run_payload_checks(
    relay_urls: list[str], beacon_url: str, start_slot: int, end_slot: int
) -> list[CheckResult]:
    """Run all payload matching checks."""
    return [check_payload_hash_match(relay_urls, beacon_url, start_slot, end_slot)]
