#!/usr/bin/env python3
"""Write the web interface's coastline: Natural Earth's 1:110m land, as one
SVG path in Web Mercator world units, for maps that work without a network.

    npm pack world-atlas@2 && tar xzf world-atlas-2.*.tgz
    python3 tools/land.py package/land-110m.json > crates/hm-cli/src/node/web/js/land.js

Natural Earth is in the public domain; world-atlas (ISC) converts it to
TopoJSON. The path uses relative moves between integer points on a world
WORLD units wide (the same projection as js/geo.js), so it stays small.
"""
import json
import math
import sys

WORLD = 8192
MAX_LAT = 85.0511287798


def project(lon, lat):
    lat = max(-MAX_LAT, min(MAX_LAT, lat))
    x = (lon + 180.0) / 360.0 * WORLD
    y = (1.0 - math.log(math.tan(math.pi / 4 + math.radians(lat) / 2)) / math.pi) / 2.0 * WORLD
    return round(x), round(y)


def arcs(topology):
    """Each arc as absolute (lon, lat) points."""
    (sx, sy), (tx, ty) = topology["transform"]["scale"], topology["transform"]["translate"]
    out = []
    for arc in topology["arcs"]:
        x = y = 0
        points = []
        for dx, dy in arc:
            x, y = x + dx, y + dy
            points.append((x * sx + tx, y * sy + ty))
        out.append(points)
    return out


def ring(indices, decoded):
    points = []
    for i in indices:
        arc = decoded[i] if i >= 0 else decoded[~i][::-1]
        points.extend(arc[1:] if points else arc)
    return points


def polygons(geometry):
    if geometry["type"] == "Polygon":
        return [geometry["arcs"]]
    if geometry["type"] == "MultiPolygon":
        return geometry["arcs"]
    if geometry["type"] == "GeometryCollection":
        return [p for g in geometry["geometries"] for p in polygons(g)]
    return []


def main(path):
    topology = json.load(open(path))
    decoded = arcs(topology)
    parts = []
    at = (0, 0)
    for polygon in polygons(topology["objects"]["land"]):
        for indices in polygon:
            points = []
            for lon, lat in ring(indices, decoded):
                p = project(lon, lat)
                if not points or p != points[-1]:
                    points.append(p)
            if len(points) < 3:
                continue
            (x, y), rest = points[0], points[1:]
            parts.append(f"m{x - at[0]} {y - at[1]}")
            steps = []
            for px, py in rest:
                steps.append(f"{px - x} {py - y}")
                x, y = px, py
            parts.append("l" + " ".join(steps) + "z")
            # After z the pen is back at the ring's first point.
            at = points[0]
    print("// Natural Earth 1:110m land (public domain, via world-atlas, ISC),")
    print(f"// in Web Mercator units of a world {WORLD} wide. Written by tools/land.py.")
    print(f"export const WORLD = {WORLD};")
    print(f'export const LAND = "{"".join(parts)}";')


if __name__ == "__main__":
    main(sys.argv[1])
