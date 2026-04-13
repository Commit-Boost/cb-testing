"""
Kurtosis enclave service discovery module.

Discovers services running in a Kurtosis enclave by invoking the kurtosis CLI
and parsing its output. No external dependencies beyond stdlib.
"""

import logging
import re
import subprocess
from dataclasses import dataclass, field
from typing import List, Optional

logger = logging.getLogger(__name__)


@dataclass
class EnclaveServices:
    """Discovered services from a Kurtosis enclave."""
    beacon_urls: List[str] = field(default_factory=list)
    relay_urls: List[str] = field(default_factory=list)
    cb_pbs_urls: List[str] = field(default_factory=list)
    cb_metrics_urls: List[str] = field(default_factory=list)
    prometheus_url: Optional[str] = None
    cb_service_names: List[str] = field(default_factory=list)


def _run_kurtosis(*args: str, timeout: int = 30) -> Optional[str]:
    """Run a kurtosis CLI command and return stdout, or None on failure."""
    cmd = ["kurtosis"] + list(args)
    logger.debug("Running: %s", " ".join(cmd))
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        if result.returncode != 0:
            stderr = result.stderr.strip()
            logger.warning("kurtosis command failed (rc=%d): %s", result.returncode, stderr)
            return None
        return result.stdout
    except FileNotFoundError:
        logger.error("kurtosis CLI not found. Is it installed and on PATH?")
        return None
    except subprocess.TimeoutExpired:
        logger.error("kurtosis command timed out after %ds", timeout)
        return None
    except OSError as exc:
        logger.error("Failed to run kurtosis: %s", exc)
        return None


def port_print(enclave: str, service: str, port_name: str) -> Optional[str]:
    """Get a clean URL for a specific service port via `kurtosis port print`."""
    output = _run_kurtosis("port", "print", enclave, service, port_name)
    if output is None:
        return None
    url = output.strip()
    if not url:
        return None
    # Ensure it has a scheme
    if not url.startswith("http"):
        url = "http://" + url
    return url


def _parse_services(inspect_output: str) -> list:
    """
    Parse `kurtosis enclave inspect` output to extract service names and their ports.

    Returns a list of (service_name, {port_name: url}) tuples.
    """
    services = []
    # The inspect output has a section starting with a line like:
    #   ========================================== User Services ==========================================
    # followed by a header line with UUID, Name, Ports, Status
    # then rows like:
    #   abcd1234   cl-lighthouse-geth   http: 4000/tcp -> http://127.0.0.1:32811   RUNNING

    in_services_section = False
    header_seen = False

    for line in inspect_output.splitlines():
        stripped = line.strip()

        # Detect the services section
        if "User Services" in stripped:
            in_services_section = True
            header_seen = False
            continue

        if not in_services_section:
            continue

        # Skip separator lines
        if stripped.startswith("====") or stripped.startswith("----"):
            continue

        # Skip empty lines
        if not stripped:
            continue

        # Detect header row (contains UUID and Name columns)
        if "UUID" in stripped and "Name" in stripped:
            header_seen = True
            continue

        if not header_seen:
            continue

        # If we hit another section header, stop
        if stripped.startswith("====="):
            break

        # Parse a service row. Format varies but generally:
        # UUID   Name   Ports   Status
        # Ports can contain multiple port mappings separated by spaces or newlines
        # e.g.: http: 4000/tcp -> http://127.0.0.1:32811
        #        metrics: 8080/tcp -> http://127.0.0.1:32812

        # Split on multiple spaces to get columns
        parts = re.split(r"\s{2,}", stripped)
        if len(parts) < 3:
            continue

        # parts[0] = UUID, parts[1] = Name, parts[2..] = Ports + Status
        service_name = parts[1].strip()
        # Rejoin the rest to parse ports
        rest = "  ".join(parts[2:])

        # Extract all port mappings: port_name: NNN/tcp -> URL
        port_map = {}
        port_pattern = re.compile(
            r"(\w[\w-]*):\s*\d+/(?:tcp|udp)\s*->\s*(https?://[^\s,]+)"
        )
        for match in port_pattern.finditer(rest):
            port_name = match.group(1)
            port_url = match.group(2)
            port_map[port_name] = port_url

        if service_name:
            services.append((service_name, port_map))

    return services


def _matches_pattern(name: str, pattern: str) -> bool:
    """Check if a service name matches a glob-like pattern (only * supported)."""
    # Convert simple glob to regex
    regex = "^" + re.escape(pattern).replace(r"\*", ".*") + "$"
    return bool(re.match(regex, name))


def discover(enclave: str) -> EnclaveServices:
    """
    Discover all relevant services in a Kurtosis enclave.

    First tries `kurtosis enclave inspect` to get a full picture of services,
    then uses `kurtosis port print` for each discovered service to get clean URLs.
    Falls back to parsed URLs from inspect output if port print fails.
    """
    result = EnclaveServices()

    # Step 1: Get full service listing via inspect
    inspect_output = _run_kurtosis("enclave", "inspect", "--full-uuids", enclave)
    if inspect_output is None:
        logger.error("Could not inspect enclave '%s'. Returning empty services.", enclave)
        return result

    services = _parse_services(inspect_output)
    if not services:
        logger.warning("No services found in enclave '%s'.", enclave)
        return result

    logger.info("Found %d services in enclave '%s'.", len(services), enclave)

    # Step 2: Categorize services by name pattern and collect URLs
    for svc_name, port_map in services:

        # --- Beacon API: cl-* services, port 'http' ---
        if _matches_pattern(svc_name, "cl-*"):
            url = port_print(enclave, svc_name, "http")
            if url is None and "http" in port_map:
                url = port_map["http"]
                logger.debug("Fell back to inspect URL for beacon %s: %s", svc_name, url)
            if url:
                result.beacon_urls.append(url)
                logger.info("Beacon API discovered: %s -> %s", svc_name, url)
            else:
                logger.warning("Beacon service '%s' found but no http port available.", svc_name)

        # --- Relay Data API: mev-relay-*-api or mev-relay-api ---
        if _matches_pattern(svc_name, "mev-relay-*-api") or svc_name == "mev-relay-api":
            url = port_print(enclave, svc_name, "http")
            if url is None and "http" in port_map:
                url = port_map["http"]
                logger.debug("Fell back to inspect URL for relay %s: %s", svc_name, url)
            if url:
                result.relay_urls.append(url)
                logger.info("Relay API discovered: %s -> %s", svc_name, url)
            else:
                logger.warning("Relay service '%s' found but no http port available.", svc_name)

        # --- Commit-Boost: commit-boost-* services, ports 'pbs' and 'metrics' ---
        if _matches_pattern(svc_name, "commit-boost-*"):
            result.cb_service_names.append(svc_name)
            # PBS port
            pbs_url = port_print(enclave, svc_name, "pbs")
            if pbs_url is None and "pbs" in port_map:
                pbs_url = port_map["pbs"]
                logger.debug("Fell back to inspect URL for CB PBS %s: %s", svc_name, pbs_url)
            if pbs_url:
                result.cb_pbs_urls.append(pbs_url)
                logger.info("Commit-Boost PBS discovered: %s -> %s", svc_name, pbs_url)
            else:
                logger.warning("Commit-Boost '%s' found but no pbs port available.", svc_name)

            # Metrics port
            metrics_url = port_print(enclave, svc_name, "metrics")
            if metrics_url is None and "metrics" in port_map:
                metrics_url = port_map["metrics"]
                logger.debug("Fell back to inspect URL for CB metrics %s: %s", svc_name, metrics_url)
            if metrics_url:
                result.cb_metrics_urls.append(metrics_url)
                logger.info("Commit-Boost metrics discovered: %s -> %s", svc_name, metrics_url)
            else:
                logger.warning("Commit-Boost '%s' found but no metrics port available.", svc_name)

        # --- Prometheus ---
        if svc_name == "prometheus":
            url = port_print(enclave, svc_name, "http")
            if url is None and "http" in port_map:
                url = port_map["http"]
                logger.debug("Fell back to inspect URL for prometheus: %s", url)
            if url:
                result.prometheus_url = url
                logger.info("Prometheus discovered: %s -> %s", svc_name, url)
            else:
                logger.warning("Prometheus service found but no http port available.")

    # Summary logging
    logger.info(
        "Discovery summary: beacons=%d, relays=%d, cb_pbs=%d, cb_metrics=%d, cb_names=%d, prometheus=%s",
        len(result.beacon_urls),
        len(result.relay_urls),
        len(result.cb_pbs_urls),
        len(result.cb_metrics_urls),
        len(result.cb_service_names),
        "yes" if result.prometheus_url else "no",
    )

    if not result.beacon_urls:
        logger.warning("No beacon API services (cl-*) found.")
    if not result.relay_urls:
        logger.warning("No relay API services (mev-relay-*-api) found.")
    if not result.cb_pbs_urls:
        logger.warning("No Commit-Boost services (commit-boost-*) found.")

    return result


if __name__ == "__main__":
    import sys

    logging.basicConfig(level=logging.DEBUG, format="%(levelname)s %(name)s: %(message)s")

    if len(sys.argv) < 2:
        print("Usage: python discovery.py <enclave-name>")
        sys.exit(1)

    enclave_name = sys.argv[1]
    svc = discover(enclave_name)
    print()
    print("=== Discovered Services ===")
    print(f"Beacon URLs:      {svc.beacon_urls}")
    print(f"Relay URLs:       {svc.relay_urls}")
    print(f"CB PBS URLs:      {svc.cb_pbs_urls}")
    print(f"CB Metrics URLs:  {svc.cb_metrics_urls}")
    print(f"CB Service Names: {svc.cb_service_names}")
    print(f"Prometheus URL:   {svc.prometheus_url}")
