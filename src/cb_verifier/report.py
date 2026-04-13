"""Verification result reporting module."""

from __future__ import annotations

import json
from dataclasses import dataclass, field, asdict
from enum import Enum
from typing import List


class ResultStatus(Enum):
    PASS = "PASS"
    FAIL = "FAIL"
    WARN = "WARN"
    SKIP = "SKIP"


# ANSI color codes
_COLORS = {
    ResultStatus.PASS: "\033[32m",  # green
    ResultStatus.FAIL: "\033[31m",  # red
    ResultStatus.WARN: "\033[33m",  # yellow
    ResultStatus.SKIP: "\033[90m",  # gray
}
_RESET = "\033[0m"
_BOLD = "\033[1m"


@dataclass
class CheckResult:
    id: str
    tier: int  # 1-3
    result: ResultStatus
    detail: str
    data: dict = field(default_factory=dict)


@dataclass
class VerificationReport:
    enclave: str
    timestamp: str  # ISO format
    observation_window: dict  # {start_slot, end_slot}
    result: ResultStatus  # PASS or FAIL
    checks: List[CheckResult] = field(default_factory=list)


def format_terminal(report: VerificationReport) -> str:
    """Format report with ANSI colored output for terminal display."""
    lines: list[str] = []
    lines.append(f"{_BOLD}Verification Report: {report.enclave}{_RESET}")
    lines.append(f"Timestamp: {report.timestamp}")
    lines.append(
        f"Observation window: slot {report.observation_window.get('start_slot', '?')}"
        f" -> {report.observation_window.get('end_slot', '?')}"
    )
    lines.append("")

    passed = failed = warnings = skipped = 0
    for c in report.checks:
        color = _COLORS.get(c.result, "")
        tag = f"{color}{c.result.value:4s}{_RESET}"
        lines.append(f"  [{tag}] {c.id} - {c.detail}")
        if c.result == ResultStatus.FAIL and c.data:
            for k, v in c.data.items():
                lines.append(f"         {k}: {v}")
        if c.result == ResultStatus.PASS:
            passed += 1
        elif c.result == ResultStatus.FAIL:
            failed += 1
        elif c.result == ResultStatus.WARN:
            warnings += 1
        else:
            skipped += 1

    lines.append("")
    overall_color = _COLORS[report.result]
    lines.append(
        f"{_BOLD}Result: {overall_color}{report.result.value}{_RESET}  "
        f"({passed} passed, {failed} failed, {warnings} warnings)"
    )
    return "\n".join(lines)


def _serialize(obj):
    """JSON serialization helper."""
    if isinstance(obj, Enum):
        return obj.value
    if isinstance(obj, (list, tuple)):
        return [_serialize(i) for i in obj]
    if isinstance(obj, dict):
        return {k: _serialize(v) for k, v in obj.items()}
    if hasattr(obj, '__dataclass_fields__'):
        return {k: _serialize(v) for k, v in asdict(obj).items()}
    return obj


def format_json(report: VerificationReport) -> str:
    """JSON serialization of the report."""
    return json.dumps(_serialize(report), indent=2)


def print_report(report: VerificationReport, json_mode: bool = False) -> None:
    """Print report to stdout."""
    if json_mode:
        print(format_json(report))
    else:
        print(format_terminal(report))


def exit_code(report: VerificationReport) -> int:
    """Determine exit code: 0=all tier-1 passed, 1=any tier-1 failed, 2=setup failure."""
    tier1 = [c for c in report.checks if c.tier == 1]
    if not tier1:
        return 2  # no tier-1 checks means setup failure
    for c in tier1:
        if c.result == ResultStatus.FAIL:
            return 1
    return 0
