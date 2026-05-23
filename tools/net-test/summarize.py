#!/usr/bin/env python3
"""Summarize a wisp-net-test session directory.

Reads every *.jsonl file in the session dir, counts events by (source, kind),
and dumps key correlations the harness cares about:

  - Did every observer receive a `connected` event?
  - For each `lantern_spawned` on the server, which observers got a
    `replicated_lantern` event and how long did it take?
  - Position deltas: for each entity replicated to multiple observers, max
    |position| spread across peers at the last frame each saw it.

Output is JSON on stdout. Lines from the per-process logs aren't merged
here — read the individual files if you need to walk the timeline event
by event.
"""

from __future__ import annotations

import json
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any


def load_session(session_dir: Path) -> dict[str, list[dict]]:
    """Return {src: [event, ...]} sorted by t."""
    by_src: dict[str, list[dict]] = defaultdict(list)
    for jsonl in sorted(session_dir.glob("*.jsonl")):
        for line in jsonl.read_text().splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            src = ev.get("src", jsonl.stem)
            by_src[src].append(ev)
    for src in by_src:
        by_src[src].sort(key=lambda e: e.get("t", 0.0))
    return dict(by_src)


def counts(by_src: dict[str, list[dict]]) -> dict[str, dict[str, int]]:
    out: dict[str, dict[str, int]] = {}
    for src, events in by_src.items():
        ctr: dict[str, int] = defaultdict(int)
        for ev in events:
            ctr[ev.get("kind", "?")] += 1
        out[src] = dict(sorted(ctr.items()))
    return out


def correlate_lanterns(by_src: dict[str, list[dict]]) -> list[dict[str, Any]]:
    """For each server lantern_spawned, find the matching replicated_lantern
    on each observer and report the receive latency."""
    server_events = by_src.get("server", [])
    lanterns = [e for e in server_events if e.get("kind") == "lantern_spawned"]
    out = []
    for ls in lanterns:
        spawn_t = ls.get("t")
        client_id = ls.get("client_id")
        pos = ls.get("pos")
        receivers = []
        for src, events in by_src.items():
            if src == "server":
                continue
            arrival = next(
                (e for e in events if e.get("kind") == "replicated_lantern" and e.get("t", 0) >= spawn_t),
                None,
            )
            if arrival is not None:
                receivers.append(
                    {
                        "src": src,
                        "t": arrival.get("t"),
                        "latency_s": round(arrival.get("t", 0) - spawn_t, 4),
                    }
                )
        out.append(
            {
                "spawned_at": spawn_t,
                "spawned_by_client_id": client_id,
                "pos": pos,
                "receivers": receivers,
            }
        )
    return out


def correlate_portals(by_src: dict[str, list[dict]]) -> list[dict[str, Any]]:
    server_events = by_src.get("server", [])
    portals = [e for e in server_events if e.get("kind") == "portal_placed"]
    out = []
    for pp in portals:
        spawn_t = pp.get("t")
        slot = pp.get("slot")
        receivers = []
        for src, events in by_src.items():
            if src == "server":
                continue
            arrival = next(
                (
                    e
                    for e in events
                    if e.get("kind") == "replicated_portal"
                    and e.get("slot") == slot
                    and e.get("t", 0) >= spawn_t
                ),
                None,
            )
            if arrival is not None:
                receivers.append(
                    {
                        "src": src,
                        "t": arrival.get("t"),
                        "latency_s": round(arrival.get("t", 0) - spawn_t, 4),
                    }
                )
        out.append(
            {
                "spawned_at": spawn_t,
                "slot": slot,
                "pos": pp.get("pos"),
                "receivers": receivers,
            }
        )
    return out


def position_spread(by_src: dict[str, list[dict]]) -> dict[str, dict[str, Any]]:
    """Compare the last position each observer saw for each replicated
    entity. If two observers diverge by more than EPSILON, that's a sync
    bug. Server is included if it traces positions; today it does not, so
    this is observer-vs-observer divergence."""
    EPSILON = 0.1
    # Map entity (string id from the trace) → src → last pos.
    last: dict[str, dict[str, list[float]]] = defaultdict(dict)
    for src, events in by_src.items():
        for ev in events:
            kind = ev.get("kind", "")
            if kind not in (
                "player_position",
                "prop_position",
                "lantern_position",
                "portal_position",
            ):
                continue
            entity = ev.get("entity", "")
            pos = ev.get("pos")
            if not entity or not pos:
                continue
            last[entity][src] = pos
    out: dict[str, dict[str, Any]] = {}
    for entity, by_src_pos in last.items():
        srcs = list(by_src_pos.keys())
        if len(srcs) < 2:
            continue
        max_delta = 0.0
        worst_pair = None
        for i, a in enumerate(srcs):
            for b in srcs[i + 1 :]:
                pa, pb = by_src_pos[a], by_src_pos[b]
                d = max(abs(pa[i] - pb[i]) for i in range(3))
                if d > max_delta:
                    max_delta = d
                    worst_pair = (a, b)
        out[entity] = {
            "observers": by_src_pos,
            "max_axis_delta": round(max_delta, 4),
            "diverged": max_delta > EPSILON,
            "worst_pair": worst_pair,
        }
    return out


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: summarize.py <session_dir>", file=sys.stderr)
        return 2
    session_dir = Path(argv[1])
    by_src = load_session(session_dir)
    summary = {
        "session_dir": str(session_dir),
        "sources": list(by_src.keys()),
        "event_counts": counts(by_src),
        "lanterns": correlate_lanterns(by_src),
        "portals": correlate_portals(by_src),
        "position_divergence": position_spread(by_src),
    }
    print(json.dumps(summary, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
