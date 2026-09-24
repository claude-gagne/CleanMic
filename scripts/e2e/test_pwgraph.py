#!/usr/bin/env python3
"""Offline unit tests for pwgraph.py's link audit against synthetic pw-dump
fixtures. No network, no PipeWire, no X server. Runs both as:

  python3 -m pytest -q -p no:cacheprovider scripts/e2e
  python3 scripts/e2e/test_pwgraph.py
"""

from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import pwgraph  # noqa: E402


def _node(node_id, name, media_class, description="", application_name=""):
    return {
        "id": node_id,
        "type": "PipeWire:Interface:Node",
        "info": {
            "props": {
                "node.name": name,
                "media.class": media_class,
                "node.description": description,
                "application.name": application_name,
            }
        },
    }


def _link(link_id, out_id, in_id):
    return {
        "id": link_id,
        "type": "PipeWire:Interface:Link",
        "info": {"output-node-id": out_id, "input-node-id": in_id},
    }


def _clean_graph():
    nodes = [
        _node(1, "cmtest_play", "Stream/Output/Audio"),
        _node(2, "cmtest_in", "Stream/Input/Audio"),
        _node(3, "cmtest_mic", "Audio/Source"),
        _node(4, "CleanMic-capture", "Stream/Input/Audio"),
        _node(5, "cmtest_rec", "Stream/Input/Audio"),
        _node(6, "CleanMic", "Audio/Source/Virtual"),
    ]
    links = [
        _link(101, 1, 2),  # cmtest_play -> cmtest_in
        _link(102, 3, 4),  # cmtest_mic -> CleanMic-capture
        _link(103, 3, 5),  # cmtest_mic -> cmtest_rec
        _link(104, 6, 5),  # CleanMic -> cmtest_rec
    ]
    return nodes + links


def test_count_and_nodes():
    dump = _clean_graph()
    assert pwgraph.count(dump, "cmtest_") == 4
    assert pwgraph.node_names(dump, "cmtest_") == [
        "cmtest_in",
        "cmtest_mic",
        "cmtest_play",
        "cmtest_rec",
    ]


def test_audit_clean_graph_passes():
    violations = pwgraph.audit(_clean_graph())
    assert violations == []


def test_audit_flags_rdp_capture_on_null_sink_monitor():
    dump = _clean_graph()
    dump.append(_node(10, "cmtest_null", "Audio/Sink"))
    dump.append(_node(11, "GRD::RDP::AUDIO_PLAYBACK", "Stream/Input/Audio"))
    dump.append(_link(110, 10, 11))
    violations = pwgraph.audit(dump, allow_sinks=["cmtest_null"])
    assert any("cmtest_null" in v and "RDP" in v for v in violations)


def test_audit_flags_player_linked_to_real_hardware():
    dump = _clean_graph()
    dump.append(_node(20, "alsa_output.pci-0000_00_1f.3.analog-stereo", "Audio/Sink"))
    # Replace the harness link with a foreign one from cmtest_play.
    dump.append(_link(120, 1, 20))
    violations = pwgraph.audit(dump)
    assert any("cmtest_play" in v and "alsa_output" in v for v in violations)


def test_audit_flags_unallowed_cmtest_audio_sink():
    dump = _clean_graph()
    dump.append(_node(30, "cmtest_sink", "Audio/Sink"))
    violations = pwgraph.audit(dump)
    assert any("cmtest_sink" in v for v in violations)


def test_audit_allows_cmtest_sink_when_explicitly_allowed():
    dump = _clean_graph()
    dump.append(_node(30, "cmtest_null", "Audio/Sink"))
    violations = pwgraph.audit(dump, allow_sinks=["cmtest_null"])
    assert violations == []


def test_audit_flags_unknown_app_linked_to_cmtest_mic():
    dump = _clean_graph()
    dump.append(_node(40, "zoom", "Stream/Input/Audio", application_name="Zoom"))
    dump.append(_link(140, 3, 40))  # cmtest_mic -> zoom, not an allowed pair
    violations = pwgraph.audit(dump)
    assert any("zoom" in v for v in violations)


def test_audit_unreadable_input_exit_code():
    rc = pwgraph.main(["--file", "/nonexistent/path/does-not-exist.json", "audit"])
    assert rc == 2


def _run_all():
    failures = []
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"PASS: {name}")
            except AssertionError as exc:
                failures.append(name)
                print(f"FAIL: {name}: {exc}")
    if failures:
        print(f"\n{len(failures)} failed: {', '.join(failures)}")
        raise SystemExit(1)
    print("\nall tests passed")


if __name__ == "__main__":
    _run_all()
