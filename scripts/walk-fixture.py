#!/usr/bin/env python3
"""Writes tests/fixtures/walk.json: a thousand kept fixes of a walk over Warsaw's real streets,
sampled every ten metres along the walking and running legs of fixtures/demo_routes.json with a little sideways
noise, as the phone's thinning would keep them. Deterministic, so the file never changes by
itself; run it only when the routes fixture changes.

    python3 scripts/walk-fixture.py
"""
import json
import math
import pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
routes = json.loads((ROOT / "fixtures" / "demo_routes.json").read_text())
LEGS = sorted(name for name, leg in routes["legs"].items() if leg["mode"] in ("walk", "run"))
STEP_M = 10.0
WANT = 1000


def metres(a, b):
    radius = 6_371_008.8
    lat1, lat2 = math.radians(a[0]), math.radians(b[0])
    d_lat = lat2 - lat1
    d_lon = math.radians(b[1] - a[1])
    h = math.sin(d_lat / 2) ** 2 + math.cos(lat1) * math.cos(lat2) * math.sin(d_lon / 2) ** 2
    return 2 * radius * math.asin(min(1.0, math.sqrt(h)))


seed = 0x9E3779B97F4A7C15


def noise():
    global seed
    seed = (seed * 6364136223846793005 + 1442695040888963407) % (1 << 64)
    return (seed >> 11) / float(1 << 53) * 2 - 1


points = []
for name in LEGS:
    coords = routes["legs"][name]["coordinates"]
    carry = 0.0
    for a, b in zip(coords, coords[1:]):
        length = metres(a, b)
        if length == 0:
            continue
        pos = carry
        while pos <= length and len(points) < WANT:
            t = pos / length
            lat = a[0] + (b[0] - a[0]) * t
            lon = a[1] + (b[1] - a[1]) * t
            # Up to 1.5 m sideways, as a phone on a pavement wobbles.
            side = noise() * 1.5
            bearing = math.atan2(
                math.radians(b[1] - a[1]) * math.cos(math.radians(lat)), math.radians(b[0] - a[0])
            )
            lat += side * math.cos(bearing + math.pi / 2) / 111_320.0
            lon += side * math.sin(bearing + math.pi / 2) / (111_320.0 * math.cos(math.radians(lat)))
            points.append([round(lat, 7), round(lon, 7)])
            pos += STEP_M
        carry = pos - length
    if len(points) >= WANT:
        break

assert len(points) == WANT, len(points)
out = ROOT / "tests" / "fixtures" / "walk.json"
out.write_text(json.dumps(points, separators=(",", ":")) + "\n")
print(f"{out}: {len(points)} points")
