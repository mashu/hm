// Maps that need no network: the Natural Earth coastline built in, the
// Maidenhead grid over it, pan and zoom with a mouse, a finger or keys.
// Nothing is fetched from anywhere, so the map works when the internet is
// down (when HF is most needed) and tells no tile server where you look.
//
// `SvgMap` is the map; `NetworkMap` draws stations and paths on one,
// `LocatorPicker` lets you pick your own square on one.

import { s } from "./dom.js";
import { hideTip, tipped } from "./charts.js";
import { bearer, km, pct, relative } from "./format.js";
import { LAND } from "./land.js";
import { WORLD, distance, greatCircle, locatorCentre, project, unproject } from "./geo.js";

/** Most zoom: screen pixels per world unit (about 5 km at the equator). */
const MAX_SCALE = 30;
const FIELD = WORLD / 18;
const SQUARE = WORLD / 180;

/** Grid lines in world units: fields (20° × 10°) or squares (2° × 1°). */
function gridPath(lonStep, latStep) {
  const lines = [];
  for (let lon = -180; lon <= 180; lon += lonStep) {
    const x = ((lon + 180) / 360) * WORLD;
    lines.push(`M${x} 0V${WORLD}`);
  }
  for (let lat = -80; lat <= 80; lat += latStep) {
    lines.push(`M0 ${project(lat, 0)[1].toFixed(1)}H${WORLD}`);
  }
  return lines.join("");
}

export class SvgMap {
  constructor(container, label) {
    this.k = 1;
    this.tx = 0;
    this.ty = 0;
    this.drawers = [];
    this.world = s("g", { class: "world" },
      s("path", { class: "land", d: LAND }),
      s("path", { class: "grid field", d: gridPath(20, 10) }),
      (this.squares = s("path", { class: "grid square", d: gridPath(2, 1) })));
    this.layers = s("g", { class: "world" });
    this.labels = s("g", { class: "grid-labels" });
    this.overlay = s("g");
    this.svg = s("svg", { class: "map", role: "application", tabindex: 0, "aria-label": label },
      s("rect", { class: "sea", width: "100%", height: "100%" }),
      this.world, this.layers, this.labels, this.overlay);
    container.replaceChildren(this.svg);
    this.#interact();
    this.resize = new ResizeObserver(() => this.#resized());
    this.resize.observe(this.svg);
  }

  get width() {
    return this.svg.clientWidth || 600;
  }

  get height() {
    return this.svg.clientHeight || 400;
  }

  /** Screen position of a place. */
  at(lat, lon) {
    const [x, y] = project(lat, lon);
    return [this.tx + this.k * x, this.ty + this.k * y];
  }

  /** The place at a screen position. */
  place(x, y) {
    return unproject((x - this.tx) / this.k, (y - this.ty) / this.k);
  }

  /** `draw()` is called after every change of view, to place what is drawn on screen. */
  onDraw(draw) {
    this.drawers.push(draw);
  }

  /** Show the places (world units, [x, y]), or the world when there are none. */
  fit(points, most = MAX_SCALE / 4) {
    // Fitted again once the map has its size, if it has none yet.
    this.fitted = [points, most];
    const [w, hgt, pad] = [this.width, this.height, 48];
    const xs = points.map((p) => p[0]);
    const ys = points.map((p) => p[1]);
    const [x0, x1, y0, y1] = points.length
      ? [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)]
      : [0, WORLD, WORLD * 0.15, WORLD * 0.85];
    const k = Math.min((w - 2 * pad) / Math.max(x1 - x0, 1e-6), (hgt - 2 * pad) / Math.max(y1 - y0, 1e-6), most);
    this.k = Math.max(this.#least(), k);
    this.tx = w / 2 - (this.k * (x0 + x1)) / 2;
    this.ty = hgt / 2 - (this.k * (y0 + y1)) / 2;
    this.draw();
  }

  zoom(factor, x = this.width / 2, y = this.height / 2) {
    const k = Math.min(MAX_SCALE, Math.max(this.#least(), this.k * factor));
    this.tx = x - (x - this.tx) * (k / this.k);
    this.ty = y - (y - this.ty) * (k / this.k);
    this.k = k;
    this.draw();
  }

  pan(dx, dy) {
    this.tx += dx;
    this.ty += dy;
    this.draw();
  }

  draw() {
    if (this.frame) return;
    this.frame = requestAnimationFrame(() => {
      this.frame = null;
      const [w, hgt] = [this.width, this.height];
      // Keep some of the world in view.
      this.tx = Math.min(w / 2, Math.max(w / 2 - this.k * WORLD, this.tx));
      this.ty = Math.min(hgt / 2, Math.max(hgt / 2 - this.k * WORLD, this.ty));
      const transform = `translate(${this.tx} ${this.ty}) scale(${this.k})`;
      this.world.setAttribute("transform", transform);
      this.layers.setAttribute("transform", transform);
      this.squares.classList.toggle("shown", this.k * SQUARE > 18);
      this.#gridLabels();
      for (const draw of this.drawers) draw();
    });
  }

  destroy() {
    this.resize.disconnect();
    hideTip();
  }

  #least() {
    return Math.min(this.width, this.height) / WORLD;
  }

  #resized() {
    const [w, hgt] = [this.svg.clientWidth, this.svg.clientHeight];
    if (!w || !hgt) return;
    if (!this.size && this.fitted) {
      this.size = [w, hgt];
      this.fit(...this.fitted);
      return;
    }
    if (this.size) {
      // Keep the centre where it was.
      this.tx += (w - this.size[0]) / 2;
      this.ty += (hgt - this.size[1]) / 2;
    }
    this.size = [w, hgt];
    this.draw();
  }

  /** Field names when fields are large enough, square names when squares are. */
  #gridLabels() {
    const [w, hgt] = [this.width, this.height];
    const labels = [];
    const cell = this.k * SQUARE > 44 ? [2, 1] : this.k * FIELD > 60 ? [20, 10] : null;
    if (cell) {
      const [dLon, dLat] = cell;
      const nw = this.place(0, 0);
      const se = this.place(w, hgt);
      const lon0 = Math.floor((nw.lon + 180) / dLon) * dLon - 180;
      const lat0 = Math.floor((se.lat + 90) / dLat) * dLat - 90;
      for (let lon = lon0; lon < se.lon && labels.length < 200; lon += dLon) {
        for (let lat = lat0; lat < nw.lat && labels.length < 200; lat += dLat) {
          if (lon < -180 || lon >= 180 || lat < -90 || lat >= 90) continue;
          const [x, y] = this.at(lat + dLat, lon);
          const x0 = lon + 180;
          const y0 = lat + 90;
          const name = String.fromCharCode(65 + Math.floor(x0 / 20), 65 + Math.floor(y0 / 10)) +
            (dLon === 2 ? String(Math.floor((x0 % 20) / 2)) + String(Math.floor(y0 % 10)) : "");
          labels.push(s("text", { x: x + 4, y: y + 12 }, name));
        }
      }
    }
    this.labels.replaceChildren(...labels);
  }

  #interact() {
    const svg = this.svg;
    const pointers = new Map();
    let moved = 0;
    let spread = 0;
    svg.addEventListener("pointerdown", (e) => {
      if (e.button !== 0) return;
      pointers.set(e.pointerId, [e.clientX, e.clientY]);
      moved = 0;
      if (pointers.size === 2) spread = this.#spread(pointers);
    });
    svg.addEventListener("pointermove", (e) => {
      const last = pointers.get(e.pointerId);
      if (!last) return;
      const [dx, dy] = [e.clientX - last[0], e.clientY - last[1]];
      pointers.set(e.pointerId, [e.clientX, e.clientY]);
      moved += Math.abs(dx) + Math.abs(dy);
      if (moved > 4 && !svg.hasPointerCapture(e.pointerId)) svg.setPointerCapture(e.pointerId);
      if (pointers.size === 1) {
        this.pan(dx, dy);
      } else if (pointers.size === 2) {
        const now = this.#spread(pointers);
        const box = svg.getBoundingClientRect();
        const [cx, cy] = [...pointers.values()].reduce((a, p) => [a[0] + p[0] / 2, a[1] + p[1] / 2], [0, 0]);
        if (spread > 0) this.zoom(now / spread, cx - box.left, cy - box.top);
        spread = now;
      }
    });
    const up = (e) => {
      pointers.delete(e.pointerId);
      spread = 0;
    };
    svg.addEventListener("pointerup", up);
    svg.addEventListener("pointercancel", up);
    // A click is a press that did not move.
    svg.addEventListener("click", (e) => {
      if (moved > 4) e.stopImmediatePropagation();
    }, true);
    svg.addEventListener("wheel", (e) => {
      e.preventDefault();
      const box = svg.getBoundingClientRect();
      this.zoom(Math.exp(-e.deltaY * (e.deltaMode ? 0.05 : 0.0015)), e.clientX - box.left, e.clientY - box.top);
    }, { passive: false });
    svg.addEventListener("dblclick", (e) => {
      const box = svg.getBoundingClientRect();
      this.zoom(2, e.clientX - box.left, e.clientY - box.top);
    });
    svg.addEventListener("keydown", (e) => {
      const step = 60;
      const keys = {
        "+": () => this.zoom(1.5), "=": () => this.zoom(1.5), "-": () => this.zoom(1 / 1.5),
        ArrowLeft: () => this.pan(step, 0), ArrowRight: () => this.pan(-step, 0),
        ArrowUp: () => this.pan(0, step), ArrowDown: () => this.pan(0, -step),
      };
      if (keys[e.key]) {
        e.preventDefault();
        keys[e.key]();
      }
    });
  }

  #spread(pointers) {
    const [a, b] = [...pointers.values()];
    return Math.hypot(a[0] - b[0], a[1] - b[1]);
  }
}

/** A path between places in world units, along the great circle when long. */
function pathBetween(a, b) {
  const points = distance(a, b).km > 800 ? greatCircle(a, b) : [a, b];
  return points.map((p, i) => {
    const [x, y] = project(p.lat, p.lon);
    return (i ? "L" : "M") + x.toFixed(2) + " " + y.toFixed(2);
  }).join("");
}

/**
 * The stations this one has heard of, where their beacons say they are, and
 * the paths it believes in between them: coloured by bearer, as wide as the
 * chance a handoff over them completes now, dashed where never seen open.
 */
export class NetworkMap {
  constructor(container, onSelect) {
    this.map = new SvgMap(container, "Map of stations and paths; drag to pan, scroll or pinch to zoom");
    this.onSelect = onSelect;
    this.stations = [];
    this.selected = null;
    this.fitted = false;
    this.map.onDraw(() => this.#drawStations());
  }

  show(insight, selected) {
    this.selected = selected;
    const here = new Map(insight.stations.filter((st) => st.place).map((st) => [st.call, st]));
    this.stations = [...here.values()];
    const me = insight.stations.find((st) => st.me);
    const links = insight.links
      .filter((l) => here.has(l.a) && here.has(l.b))
      .sort((x, y) => x.success - y.success);
    const paths = links.map((l) => {
      const [a, b] = [here.get(l.a).place, here.get(l.b).place];
      const d = pathBetween(a, b);
      const lines = () => [
        `${l.a} – ${l.b}`,
        `${bearer(l.bearer).label}, ${km(distance(a, b).km)}`,
        `Handoff now: ${pct(l.success)}`,
        `Open now: ${pct(l.open_now)}${l.seen ? "" : " (never seen open)"}`,
        l.next_opening && l.open_now < 0.5 ? `Likely open ${relative(l.next_opening, insight.at)}` : null,
      ].filter(Boolean);
      const touches = selected && (l.a === selected || l.b === selected);
      return tipped(s("g", { class: ["path", "b-" + l.bearer, l.seen ? null : "inferred", touches ? "selected" : null] },
        s("path", { class: "hit", d }),
        s("path", { class: "stroke", d, style: { strokeWidth: (1 + 5 * l.success).toFixed(1) + "px" } })), lines);
    });
    this.map.layers.replaceChildren(...paths);
    if (!this.fitted) {
      this.fitted = true;
      const points = this.stations.map((st) => project(st.place.lat, st.place.lon));
      this.map.fit(points.length ? points : me?.place ? [project(me.place.lat, me.place.lon)] : []);
    } else {
      this.map.draw();
    }
  }

  /** Centre on a station. */
  focus(call) {
    const st = this.stations.find((x) => x.call === call);
    if (!st) return;
    const [x, y] = this.map.at(st.place.lat, st.place.lon);
    this.map.pan(this.map.width / 2 - x, this.map.height / 2 - y);
  }

  destroy() {
    this.map.destroy();
  }

  #drawStations() {
    const marks = this.stations.map((st) => {
      const [x, y] = this.map.at(st.place.lat, st.place.lon);
      const key = st.beacon?.key;
      const g = s("g", {
        class: ["station", st.me ? "me" : null, st.call === this.selected ? "selected" : null, key === "mismatch" ? "mismatch" : null],
        transform: `translate(${x.toFixed(1)} ${y.toFixed(1)})`,
        tabindex: 0, role: "button", "aria-label": "Station " + st.call,
        onclick: () => this.onSelect(st.call),
        onkeydown: (e) => (e.key === "Enter" || e.key === " ") && this.onSelect(st.call),
      },
      s("circle", { r: st.me ? 7 : 5 }),
      s("text", { x: 9, y: 4 }, st.call));
      return tipped(g, () => [st.call, st.place.locator, st.me ? "This station" : st.heard_at ? "Heard " + relative(st.heard_at) : "Not heard here"]);
    });
    this.map.overlay.replaceChildren(...marks);
  }
}

/**
 * Your own square: click the map (or give a place) and a square near it is
 * chosen, within a privacy range drawn as a circle around the place.
 */
export class LocatorPicker {
  constructor(container, onPick) {
    this.map = new SvgMap(container, "Map: click to choose your grid square");
    this.shown = null;
    this.map.svg.addEventListener("click", (e) => {
      const box = this.map.svg.getBoundingClientRect();
      onPick(this.map.place(e.clientX - box.left, e.clientY - box.top));
    });
    this.map.onDraw(() => this.#drawMarker());
    this.map.fit([]);
  }

  /** Show `locator`'s square, and the range of `km` around `anchor` it was picked within. */
  show(locator, anchor, rangeKm, pan) {
    const centre = locatorCentre(locator || "");
    this.shown = centre ? { centre, locator } : null;
    const marks = [];
    if (centre) {
      const six = locator.length === 6;
      const [w, hgt] = six ? [2 / 24, 1 / 24] : [2, 1];
      const [x0, y0] = project(centre.lat + hgt / 2, centre.lon - w / 2);
      const [x1, y1] = project(centre.lat - hgt / 2, centre.lon + w / 2);
      marks.push(s("rect", { class: "picked", x: x0, y: y0, width: x1 - x0, height: y1 - y0 }));
    }
    if (anchor && rangeKm > 0) {
      const ring = [];
      for (let i = 0; i <= 48; i++) {
        const b = (i / 48) * 2 * Math.PI;
        const lat = anchor.lat + (rangeKm / 111.32) * Math.cos(b);
        const lon = anchor.lon + (rangeKm / (111.32 * Math.cos((anchor.lat * Math.PI) / 180))) * Math.sin(b);
        const [x, y] = project(lat, lon);
        ring.push((i ? "L" : "M") + x.toFixed(2) + " " + y.toFixed(2));
      }
      marks.push(s("path", { class: "range", d: ring.join("") + "Z" }));
    }
    this.map.layers.replaceChildren(...marks);
    if (pan && (centre || anchor)) {
      const around = anchor || centre;
      const reach = Math.max(rangeKm || 0, 15) * 2.5;
      const corners = [[reach, reach], [-reach, -reach]].map(([dn, de]) =>
        project(around.lat + dn / 111.32, around.lon + de / (111.32 * Math.cos((around.lat * Math.PI) / 180))));
      this.map.fit(corners, MAX_SCALE);
    } else {
      this.map.draw();
    }
  }

  destroy() {
    this.map.destroy();
  }

  #drawMarker() {
    if (!this.shown) return this.map.overlay.replaceChildren();
    const [x, y] = this.map.at(this.shown.centre.lat, this.shown.centre.lon);
    this.map.overlay.replaceChildren(
      s("g", { class: "station me", transform: `translate(${x.toFixed(1)} ${y.toFixed(1)})` },
        s("circle", { r: 6 }), s("text", { x: 9, y: 4 }, this.shown.locator)));
  }
}
