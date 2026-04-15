"""Relay pipeline verification checks for MEV pipeline stages."""

import requests
from cb_verifier.report import CheckResult, ResultStatus


def check_builder_blocks_received(relay_url: str, start_slot: int, end_slot: int) -> CheckResult:
    """Check that builder blocks were received by the relay in the given slot range.

    NOTE: The relay data API's builder_blocks_received endpoint requires at least one
    filter param (slot, block_hash, block_number, or builder_pubkey). Limit-only
    queries return 400. We sample slots across the range to collect builder blocks.
    """
    try:
        all_entries = []
        # The relay requires at least one filter param (slot, block_hash,
        # block_number, or builder_pubkey). Limit-only queries return 400.
        # Sample slots across the range to collect builder blocks.
        sample_slots = list(range(start_slot, end_slot + 1, max(1, (end_slot - start_slot) // 10)))
        for slot in sample_slots:
            try:
                resp = requests.get(
                    f"{relay_url}/relay/v1/data/bidtraces/builder_blocks_received",
                    params={"slot": slot},
                    timeout=10,
                )
                if resp.status_code == 200:
                    entries = resp.json()
                    if entries:
                        all_entries.extend(entries)
            except Exception:
                continue
        # Deduplicate across sampled slots
        slots = sorted(set(int(e["slot"]) for e in all_entries))
        count = len(all_entries)
        if count > 0:
            return CheckResult(
                id="relay.builder_blocks_received",
                tier=1,
                result=ResultStatus.PASS,
                detail=f"Received {count} builder block(s) in slot range [{start_slot}, {end_slot}]",
                data={"count": count, "slots": slots},
            )
        else:
            return CheckResult(
                id="relay.builder_blocks_received",
                tier=1,
                result=ResultStatus.FAIL,
                detail=f"No builder blocks received in slot range [{start_slot}, {end_slot}]",
                data={"count": 0, "slots": []},
            )
    except Exception as e:
        return CheckResult(
            id="relay.builder_blocks_received",
            tier=1,
            result=ResultStatus.FAIL,
            detail=f"Error querying builder blocks received: {e}",
            data={},
        )


def check_payloads_delivered(relay_url: str, start_slot: int, end_slot: int) -> CheckResult:
    """Check that proposer payloads were delivered by the relay in the given slot range."""
    try:
        resp = requests.get(
            f"{relay_url}/relay/v1/data/bidtraces/proposer_payload_delivered",
            params={"cursor": end_slot, "limit": end_slot - start_slot + 1},
            timeout=10,
        )
        resp.raise_for_status()
        entries = resp.json()
        filtered = [e for e in entries if start_slot <= int(e.get("slot", 0)) <= end_slot]
        payloads = [
            {
                "slot": int(e["slot"]),
                "block_hash": e.get("block_hash", ""),
                "proposer_pubkey": e.get("proposer_pubkey", ""),
                "value": e.get("value", "0"),
            }
            for e in filtered
        ]
        count = len(payloads)
        if count > 0:
            return CheckResult(
                id="relay.payloads_delivered",
                tier=1,
                result=ResultStatus.PASS,
                detail=f"Delivered {count} payload(s) in slot range [{start_slot}, {end_slot}]",
                data={"count": count, "payloads": payloads},
            )
        else:
            return CheckResult(
                id="relay.payloads_delivered",
                tier=1,
                result=ResultStatus.FAIL,
                detail=f"No payloads delivered in slot range [{start_slot}, {end_slot}]",
                data={"count": 0, "payloads": []},
            )
    except Exception as e:
        return CheckResult(
            id="relay.payloads_delivered",
            tier=1,
            result=ResultStatus.FAIL,
            detail=f"Error querying payloads delivered: {e}",
            data={},
        )


def check_validator_registrations(relay_url: str, pubkeys: list) -> CheckResult:
    """Check validator registrations on the relay."""
    try:
        registered = []
        missing = []
        for pubkey in pubkeys:
            try:
                resp = requests.get(
                    f"{relay_url}/relay/v1/data/validator_registration",
                    params={"pubkey": pubkey},
                    timeout=10,
                )
                if resp.status_code == 200:
                    registered.append(pubkey)
                else:
                    missing.append(pubkey)
            except Exception:
                missing.append(pubkey)

        total = len(pubkeys)
        reg_count = len(registered)
        data = {"registered": reg_count, "total": total, "missing": missing}

        if reg_count == total:
            return CheckResult(
                id="relay.validator_registrations",
                tier=3,
                result=ResultStatus.PASS,
                detail=f"All {total} validator(s) registered on relay",
                data=data,
            )
        elif reg_count > 0:
            return CheckResult(
                id="relay.validator_registrations",
                tier=3,
                result=ResultStatus.WARN,
                detail=f"{reg_count}/{total} validator(s) registered; {len(missing)} missing",
                data=data,
            )
        else:
            return CheckResult(
                id="relay.validator_registrations",
                tier=3,
                result=ResultStatus.FAIL,
                detail=f"No validators registered on relay (0/{total})",
                data=data,
            )
    except Exception as e:
        return CheckResult(
            id="relay.validator_registrations",
            tier=3,
            result=ResultStatus.FAIL,
            detail=f"Error checking validator registrations: {e}",
            data={},
        )


def check_mev_delivery_rate(
    relay_url: str,
    beacon_url: str,
    start_slot: int,
    end_slot: int,
    threshold: float = 0.30,
) -> CheckResult:
    """Check the MEV delivery rate by comparing relay payloads to beacon chain blocks."""
    try:
        # Get delivered payloads from relay
        resp = requests.get(
            f"{relay_url}/relay/v1/data/bidtraces/proposer_payload_delivered",
            params={"cursor": end_slot, "limit": end_slot - start_slot + 1},
            timeout=10,
        )
        resp.raise_for_status()
        entries = resp.json()
        delivered_hashes = set()
        for e in entries:
            slot = int(e.get("slot", 0))
            if start_slot <= slot <= end_slot:
                bh = e.get("block_hash", "")
                if bh:
                    delivered_hashes.add(bh)

        # Walk beacon blocks in the slot range
        mev_blocks = 0
        total_blocks = 0
        missed_slots = 0

        for slot in range(start_slot, end_slot + 1):
            try:
                bresp = requests.get(
                    f"{beacon_url}/eth/v2/beacon/blocks/{slot}",
                    timeout=10,
                )
                if bresp.status_code == 404:
                    missed_slots += 1
                    continue
                bresp.raise_for_status()
                block_data = bresp.json()
                exec_payload = (
                    block_data.get("data", {})
                    .get("message", {})
                    .get("body", {})
                    .get("execution_payload", {})
                )
                block_hash = exec_payload.get("block_hash", "")
                total_blocks += 1
                if block_hash in delivered_hashes:
                    mev_blocks += 1
            except Exception:
                missed_slots += 1

        rate = mev_blocks / total_blocks if total_blocks > 0 else 0.0
        data = {
            "mev_blocks": mev_blocks,
            "total_blocks": total_blocks,
            "missed_slots": missed_slots,
            "rate": round(rate, 4),
        }

        if total_blocks == 0:
            return CheckResult(
                id="relay.mev_delivery_rate",
                tier=2,
                result=ResultStatus.FAIL,
                detail="No proposed blocks found in slot range",
                data=data,
            )
        elif rate >= threshold:
            return CheckResult(
                id="relay.mev_delivery_rate",
                tier=2,
                result=ResultStatus.PASS,
                detail=f"MEV delivery rate {rate:.2%} >= threshold {threshold:.2%}",
                data=data,
            )
        else:
            return CheckResult(
                id="relay.mev_delivery_rate",
                tier=2,
                result=ResultStatus.WARN,
                detail=f"MEV delivery rate {rate:.2%} below threshold {threshold:.2%}",
                data=data,
            )
    except Exception as e:
        return CheckResult(
            id="relay.mev_delivery_rate",
            tier=2,
            result=ResultStatus.FAIL,
            detail=f"Error checking MEV delivery rate: {e}",
            data={},
        )


def check_payloads_delivered_multi(
    relay_urls: list, start_slot: int, end_slot: int
) -> CheckResult:
    """Check payloads delivered across multiple relays (union)."""
    try:
        all_payloads = {}  # slot -> payload info, deduped by slot
        for relay_url in relay_urls:
            try:
                resp = requests.get(
                    f"{relay_url}/relay/v1/data/bidtraces/proposer_payload_delivered",
                    params={"cursor": end_slot, "limit": end_slot - start_slot + 1},
                    timeout=10,
                )
                resp.raise_for_status()
                entries = resp.json()
                for e in entries:
                    slot = int(e.get("slot", 0))
                    if start_slot <= slot <= end_slot and slot not in all_payloads:
                        all_payloads[slot] = {
                            "slot": slot,
                            "block_hash": e.get("block_hash", ""),
                            "proposer_pubkey": e.get("proposer_pubkey", ""),
                            "value": e.get("value", "0"),
                        }
            except Exception:
                continue

        payloads = list(all_payloads.values())
        count = len(payloads)
        if count > 0:
            return CheckResult(
                id="relay.payloads_delivered_multi",
                tier=1,
                result=ResultStatus.PASS,
                detail=f"Delivered {count} payload(s) across {len(relay_urls)} relay(s) in slot range [{start_slot}, {end_slot}]",
                data={"count": count, "payloads": payloads},
            )
        else:
            return CheckResult(
                id="relay.payloads_delivered_multi",
                tier=1,
                result=ResultStatus.FAIL,
                detail=f"No payloads delivered across {len(relay_urls)} relay(s) in slot range [{start_slot}, {end_slot}]",
                data={"count": 0, "payloads": []},
            )
    except Exception as e:
        return CheckResult(
            id="relay.payloads_delivered_multi",
            tier=1,
            result=ResultStatus.FAIL,
            detail=f"Error querying payloads delivered (multi): {e}",
            data={},
        )


def run_relay_checks(
    relay_urls: list,
    beacon_url: str,
    start_slot: int,
    end_slot: int,
    validator_pubkeys: list = None,
    mev_threshold: float = 0.30,
) -> list:
    """Run all relay pipeline checks and return a list of CheckResults."""
    results = []
    first_relay = relay_urls[0] if relay_urls else ""

    results.append(check_builder_blocks_received(first_relay, start_slot, end_slot))
    results.append(check_payloads_delivered_multi(relay_urls, start_slot, end_slot))
    results.append(check_mev_delivery_rate(first_relay, beacon_url, start_slot, end_slot, threshold=mev_threshold))

    if validator_pubkeys:
        results.append(check_validator_registrations(first_relay, validator_pubkeys))

    return results
