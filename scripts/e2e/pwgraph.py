#!/usr/bin/env python3
"""pwgraph.py -- pw-dump JSON reader and RDP-safety link audit for the CleanMic
silent E2E harness (scripts/e2e-audio.sh, scripts/nested-run.sh).

WHY THIS EXISTS. The 2026-09-24 base-latency debug session proved that a
naive virtual-mic test graph (an Audio/Sink node fed with test speech) is
NOT silent on a machine with an active remote-desktop session:
gnome-remote-desktop-daemon attaches one GRD::RDP::AUDIO_PLAYBACK capture
stream to every Audio/Sink node the instant it appears, mirroring the test
speech to the RDP client. This module implements the guard: it reads the
live PipeWire graph (pw-dump JSON) and flags any node or link the harness
did not intend, BEFORE test audio is allowed to flow.

USAGE
  pw-dump | pwgraph.py count --prefix cmtest_
  pw-dump | pwgraph.py nodes --prefix cmtest_
  pw-dump | pwgraph.py audit [--allow-sink NAME]...
  pwgraph.py <subcommand> --file DUMP.json ...   # read from a file instead of stdin

EXIT CODES
  0  clean (or the requested count/nodes printed)
  1  audit found one or more violations
  2  input could not be parsed as pw-dump JSON
"""

from __future__ import annotations

import argparse
import json
import sys
from typing import Any


def load_dump(text: str) -> list[dict[str, Any]]:
    """Parse pw-dump's JSON array. Raises ValueError on anything else."""
    data = json.loads(text)
    if not isinstance(data, list):
        raise ValueError("pw-dump JSON root is not a list")
    return data


def index_nodes(dump: list[dict[str, Any]]) -> dict[int, dict[str, str]]:
    """Map node id -> {name, media_class, description, application_name}."""
    nodes: dict[int, dict[str, str]] = {}
    for obj in dump:
        if not str(obj.get("type", "")).endswith("Node"):
            continue
        props = ((obj.get("info") or {}).get("props") or {})
        nodes[obj.get("id")] = {
            "name": str(props.get("node.name", "")),
            "media_class": str(props.get("media.class", "")),
            "description": str(props.get("node.description", "")),
            "application_name": str(props.get("application.name", "")),
        }
    return nodes


def list_links(dump: list[dict[str, Any]]) -> list[tuple[int, int]]:
    """List (output_node_id, input_node_id) for every PipeWire link object."""
    links: list[tuple[int, int]] = []
    for obj in dump:
        if not str(obj.get("type", "")).endswith("Link"):
            continue
        info = obj.get("info") or {}
        out_id = info.get("output-node-id")
        in_id = info.get("input-node-id")
        if out_id is None or in_id is None:
            props = info.get("props") or {}
            out_id = out_id if out_id is not None else props.get("link.output.node")
            in_id = in_id if in_id is not None else props.get("link.input.node")
        if out_id is not None and in_id is not None:
            links.append((out_id, in_id))
    return links


def count(dump: list[dict[str, Any]], prefix: str) -> int:
    nodes = index_nodes(dump)
    return sum(1 for n in nodes.values() if n["name"].startswith(prefix))


def node_names(dump: list[dict[str, Any]], prefix: str) -> list[str]:
    nodes = index_nodes(dump)
    return sorted(n["name"] for n in nodes.values() if n["name"].startswith(prefix))


# Harness node names that participate in the RDP-safe graph across every
# scenario (baseline capture/playback, plus the optional --monitor-null-sink
# path). Any link touching one of these, or any cmtest_*-prefixed node, must
# match ALLOWED_PAIRS below or it is a violation.
HARNESS_NODES = {
    "cmtest_in",
    "cmtest_mic",
    "cmtest_play",
    "cmtest_rec",
    "cmtest_null",
    "cmtest_null_src",
    "CleanMic-monitor",
}

# (source node name, destination node name) pairs the harness itself creates.
# Anything else touching a harness node is foreign and gets flagged.
ALLOWED_PAIRS = {
    ("cmtest_play", "cmtest_in"),
    ("cmtest_mic", "CleanMic-capture"),
    ("cmtest_mic", "cmtest_rec"),
    ("CleanMic", "cmtest_rec"),
    ("CleanMic-monitor", "cmtest_null"),
    ("cmtest_null_src", "cmtest_rec"),
}


def _is_harness_name(name: str) -> bool:
    return name in HARNESS_NODES or name.startswith("cmtest_")


def _remote_desktop_hint(*names_and_props: str) -> str:
    blob = " ".join(names_and_props).upper()
    if "GRD" in blob or "RDP" in blob:
        return " [remote-desktop capture: this sink would be heard on the RDP client]"
    return ""


def audit(dump: list[dict[str, Any]], allow_sinks: list[str] | None = None) -> list[str]:
    """Return a list of human-readable violation strings (empty = clean)."""
    allow = set(allow_sinks or [])
    nodes = index_nodes(dump)
    violations: list[str] = []

    # Rule 1: a cmtest_* node claiming Audio/Sink must be explicitly allowed.
    for node in nodes.values():
        name = node["name"]
        if name.startswith("cmtest_") and node["media_class"] == "Audio/Sink" and name not in allow:
            violations.append(
                f"disallowed Audio/Sink node: {name} ({node['media_class']}) -- "
                "not passed via --allow-sink"
            )

    # Rule 2: every link touching a harness (or cmtest_*) node must be one of
    # the allowed pairs.
    for out_id, in_id in list_links(dump):
        out_n = nodes.get(out_id, {"name": f"id:{out_id}", "media_class": "?", "description": "", "application_name": ""})
        in_n = nodes.get(in_id, {"name": f"id:{in_id}", "media_class": "?", "description": "", "application_name": ""})
        out_name, in_name = out_n["name"], in_n["name"]
        if not (_is_harness_name(out_name) or _is_harness_name(in_name)):
            continue
        if (out_name, in_name) in ALLOWED_PAIRS:
            continue
        hint = _remote_desktop_hint(
            out_name, in_name, out_n["description"], in_n["description"],
            out_n["application_name"], in_n["application_name"],
        )
        violations.append(
            f"disallowed link: {out_name} ({out_n['media_class']}) -> "
            f"{in_name} ({in_n['media_class']}){hint}"
        )

    return violations


def _read_input(args: argparse.Namespace) -> list[dict[str, Any]]:
    if args.file:
        with open(args.file, encoding="utf-8") as fh:
            text = fh.read()
    else:
        text = sys.stdin.read()
    return load_dump(text)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--file", help="read pw-dump JSON from this file instead of stdin")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_count = sub.add_parser("count")
    p_count.add_argument("--prefix", required=True)

    p_nodes = sub.add_parser("nodes")
    p_nodes.add_argument("--prefix", required=True)

    p_audit = sub.add_parser("audit")
    p_audit.add_argument("--allow-sink", action="append", default=[])

    args = parser.parse_args(argv)

    try:
        dump = _read_input(args)
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        print(f"pwgraph: unreadable pw-dump input: {exc}", file=sys.stderr)
        return 2

    if args.cmd == "count":
        print(count(dump, args.prefix))
        return 0
    if args.cmd == "nodes":
        for name in node_names(dump, args.prefix):
            print(name)
        return 0
    if args.cmd == "audit":
        violations = audit(dump, args.allow_sink)
        if not violations:
            print("audit: clean")
            return 0
        for v in violations:
            print(f"VIOLATION: {v}")
        return 1
    parser.error(f"unknown subcommand {args.cmd!r}")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
