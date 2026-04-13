"""Commit-Boost Prometheus metrics verification."""

import logging
import re
import subprocess
import requests
from cb_verifier.report import CheckResult, ResultStatus


def parse_prometheus_text(text: str) -> dict:
    """Parse Prometheus text exposition format.

    Returns {metric_name: [{labels: {}, value: float}]}
    """
    metrics = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        # Match: metric_name{label1="val1",label2="val2"} value
        # or:    metric_name value
        m = re.match(r'^([a-zA-Z_:][a-zA-Z0-9_:]*)\{(.+?)\}\s+(.+)$', line)
        if m:
            name = m.group(1)
            labels_str = m.group(2)
            value_str = m.group(3).split()[0]  # ignore optional timestamp
            labels = {}
            for pair in re.findall(r'([a-zA-Z_][a-zA-Z0-9_]*)="((?:[^"\\]|\\.)*)"', labels_str):
                labels[pair[0]] = pair[1]
            metrics.setdefault(name, []).append({"labels": labels, "value": float(value_str)})
            continue
        m = re.match(r'^([a-zA-Z_:][a-zA-Z0-9_:]*)\s+(.+)$', line)
        if m:
            name = m.group(1)
            value_str = m.group(2).split()[0]
            try:
                val = float(value_str)
            except ValueError:
                continue
            metrics.setdefault(name, []).append({"labels": {}, "value": val})
    return metrics


logger = logging.getLogger(__name__)


def _fetch_metrics(metrics_url: str) -> dict | None:
    """Fetch and parse metrics via HTTP, returning None on failure."""
    try:
        resp = requests.get(f"{metrics_url}/metrics", timeout=5)
        resp.raise_for_status()
        return parse_prometheus_text(resp.text)
    except Exception:
        return None


def _fetch_metrics_via_exec(enclave: str, service: str, port: int = 9090) -> dict | None:
    """Fetch metrics via kurtosis service exec when the port isn't exposed to the host."""
    try:
        proc = subprocess.run(
            ["kurtosis", "service", "exec", enclave, service,
             f"curl -s http://localhost:{port}/metrics"],
            capture_output=True, text=True, timeout=30,
        )
        if proc.returncode != 0:
            logger.debug("exec metrics fetch failed: %s", proc.stderr.strip())
            return None
        # kurtosis exec prefixes output with service name; parse all lines
        text = proc.stdout
        parsed = parse_prometheus_text(text)
        if parsed:
            return parsed
        logger.debug("No metrics parsed from exec output")
        return None
    except Exception as e:
        logger.debug("exec metrics fetch error: %s", e)
        return None


def check_relay_latency(metrics_url: str, threshold_ms: float = 500.0, _metrics: dict | None = None) -> CheckResult:
    """Check relay latency from Prometheus metrics."""
    metrics = _metrics if _metrics is not None else _fetch_metrics(metrics_url)
    if metrics is None:
        return CheckResult(
            id="cb_relay_latency",
            tier=2,
            result=ResultStatus.SKIP,
            detail="Could not fetch metrics from Commit-Boost",
            data={},
        )

    sum_samples = metrics.get("cb_pbs_relay_latency_sum", [])
    count_samples = metrics.get("cb_pbs_relay_latency_count", [])

    if not sum_samples or not count_samples:
        return CheckResult(
            id="cb_relay_latency",
            tier=2,
            result=ResultStatus.SKIP,
            detail="Relay latency metrics not available",
            data={},
        )

    total_sum = sum(s["value"] for s in sum_samples)
    total_count = sum(s["value"] for s in count_samples)

    if total_count == 0:
        return CheckResult(
            id="cb_relay_latency",
            tier=2,
            result=ResultStatus.SKIP,
            detail="No relay latency observations yet (count=0)",
            data={"sum": total_sum, "count": total_count},
        )

    mean = total_sum / total_count
    if mean < threshold_ms:
        return CheckResult(
            id="cb_relay_latency",
            tier=2,
            result=ResultStatus.PASS,
            detail=f"Mean relay latency {mean:.1f}ms < {threshold_ms}ms threshold",
            data={"mean_ms": mean, "threshold_ms": threshold_ms},
        )
    else:
        return CheckResult(
            id="cb_relay_latency",
            tier=2,
            result=ResultStatus.WARN,
            detail=f"Mean relay latency {mean:.1f}ms >= {threshold_ms}ms threshold",
            data={"mean_ms": mean, "threshold_ms": threshold_ms},
        )


def check_relay_errors(metrics_url: str, _metrics: dict | None = None) -> CheckResult:
    """Check for relay 5xx errors."""
    metrics = _metrics if _metrics is not None else _fetch_metrics(metrics_url)
    if metrics is None:
        return CheckResult(
            id="cb_relay_errors",
            tier=2,
            result=ResultStatus.SKIP,
            detail="Could not fetch metrics from Commit-Boost",
            data={},
        )

    samples = metrics.get("cb_pbs_relay_status_code_total", [])
    if not samples:
        return CheckResult(
            id="cb_relay_errors",
            tier=2,
            result=ResultStatus.SKIP,
            detail="Relay status code metrics not available",
            data={},
        )

    error_count = 0.0
    for s in samples:
        code = s["labels"].get("http_status_code", "")
        if code.startswith("5"):
            error_count += s["value"]

    if error_count == 0:
        return CheckResult(
            id="cb_relay_errors",
            tier=2,
            result=ResultStatus.PASS,
            detail="No relay 5xx errors detected",
            data={"5xx_total": error_count},
        )
    else:
        return CheckResult(
            id="cb_relay_errors",
            tier=2,
            result=ResultStatus.WARN,
            detail=f"Relay 5xx errors detected: {error_count:.0f} total",
            data={"5xx_total": error_count},
        )


def check_header_values(metrics_url: str, _metrics: dict | None = None) -> CheckResult:
    """Check that relay header values are non-zero."""
    metrics = _metrics if _metrics is not None else _fetch_metrics(metrics_url)
    if metrics is None:
        return CheckResult(
            id="cb_header_values",
            tier=2,
            result=ResultStatus.SKIP,
            detail="Could not fetch metrics from Commit-Boost",
            data={},
        )

    samples = metrics.get("cb_pbs_relay_header_value", [])
    if not samples:
        return CheckResult(
            id="cb_header_values",
            tier=2,
            result=ResultStatus.SKIP,
            detail="Relay header value metrics not available",
            data={},
        )

    has_nonzero = any(s["value"] > 0 for s in samples)
    if has_nonzero:
        return CheckResult(
            id="cb_header_values",
            tier=2,
            result=ResultStatus.PASS,
            detail="Relay header values contain non-zero entries",
            data={"sample_count": len(samples)},
        )
    else:
        return CheckResult(
            id="cb_header_values",
            tier=2,
            result=ResultStatus.WARN,
            detail="All relay header values are zero",
            data={"sample_count": len(samples)},
        )


def check_get_header_success(metrics_url: str, _metrics: dict | None = None) -> CheckResult:
    """Check for successful get_header responses."""
    metrics = _metrics if _metrics is not None else _fetch_metrics(metrics_url)
    if metrics is None:
        return CheckResult(
            id="cb_get_header_success",
            tier=2,
            result=ResultStatus.SKIP,
            detail="Could not fetch metrics from Commit-Boost",
            data={},
        )

    samples = metrics.get("cb_pbs_beacon_node_status_code_total", [])
    if not samples:
        return CheckResult(
            id="cb_get_header_success",
            tier=2,
            result=ResultStatus.SKIP,
            detail="Beacon node status code metrics not available",
            data={},
        )

    success_count = 0.0
    for s in samples:
        if (s["labels"].get("endpoint") == "get_header"
                and s["labels"].get("http_status_code") == "200"):
            success_count += s["value"]

    if success_count > 0:
        return CheckResult(
            id="cb_get_header_success",
            tier=2,
            result=ResultStatus.PASS,
            detail=f"get_header 200 responses: {success_count:.0f}",
            data={"success_count": success_count},
        )
    else:
        return CheckResult(
            id="cb_get_header_success",
            tier=2,
            result=ResultStatus.WARN,
            detail="No successful get_header (200) responses found",
            data={"success_count": 0},
        )


def run_metrics_checks(
    metrics_url: str | None,
    enclave: str | None = None,
    cb_services: list[str] | None = None,
    metrics_port: int = 9090,
) -> list:
    """Run all metrics checks.

    If metrics_url is available, use HTTP. Otherwise fall back to
    kurtosis service exec to curl metrics from inside the container.
    """
    check_ids = [
        "cb_relay_latency",
        "cb_relay_errors",
        "cb_header_values",
        "cb_get_header_success",
    ]

    # Try HTTP first
    if metrics_url is not None:
        return [
            check_relay_latency(metrics_url),
            check_relay_errors(metrics_url),
            check_header_values(metrics_url),
            check_get_header_success(metrics_url),
        ]

    # Fall back to exec if we have enclave + service info
    if enclave and cb_services:
        service = cb_services[0]
        logger.info("Metrics port not exposed; fetching via exec from %s", service)
        metrics = _fetch_metrics_via_exec(enclave, service, metrics_port)
        if metrics is not None:
            return [
                check_relay_latency('exec://prefetched', _metrics=metrics),
                check_relay_errors('exec://prefetched', _metrics=metrics),
                check_header_values('exec://prefetched', _metrics=metrics),
                check_get_header_success('exec://prefetched', _metrics=metrics),
            ]
        else:
            return [
                CheckResult(
                    id=cid, tier=2, result=ResultStatus.SKIP,
                    detail="Metrics not available (CB needs CB_METRICS_PORT env var; not set in kurtosis PBS mode)",
                    data={},
                )
                for cid in check_ids
            ]

    return [
        CheckResult(
            id=cid, tier=2, result=ResultStatus.SKIP,
            detail="No metrics URL and no enclave info for exec fallback",
            data={},
        )
        for cid in check_ids
    ]
