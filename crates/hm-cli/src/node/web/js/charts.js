// Small SVG charts of beliefs: chances over the day, credible intervals,
// how given chances came true, how one observation moved a chance. Every
// mark has a hover tooltip; colours come from the stylesheet by role.

import { h, s } from "./dom.js";
import { count, hhmm, pct } from "./format.js";

// ---- Tooltip ---------------------------------------------------------------

let tip = null;

/** Show `lines` (strings; the first in bold) next to the pointer. */
export function showTip(event, lines) {
  if (!tip) {
    tip = h("div", { class: "tip", role: "tooltip" });
    document.body.append(tip);
  }
  tip.replaceChildren(...lines.map((line, i) => h(i === 0 ? "strong" : "span", null, line)));
  tip.hidden = false;
  const pad = 14;
  const { innerWidth: w, innerHeight: hgt } = window;
  const box = tip.getBoundingClientRect();
  const x = event.clientX + pad + box.width > w ? event.clientX - pad - box.width : event.clientX + pad;
  const y = event.clientY + pad + box.height > hgt ? event.clientY - pad - box.height : event.clientY + pad;
  tip.style.left = Math.max(4, x) + "px";
  tip.style.top = Math.max(4, y) + "px";
}

export function hideTip() {
  if (tip) tip.hidden = true;
}

/** Give `node` a tooltip of `lines()`. */
export function tipped(node, lines) {
  node.addEventListener("pointermove", (e) => showTip(e, lines()));
  node.addEventListener("pointerleave", hideTip);
  return node;
}

// ---- Scales ------------------------------------------------------------------

/** One of seven steps of the sequential ramp for a chance. */
export const seq = (p) => "seq-" + Math.min(6, Math.max(0, Math.floor(p * 7)));

// ---- Chance over the next day ------------------------------------------------

/**
 * `values[i]`: chance at `start + i` hours. An area under a line, with the
 * UTC hour on the axis and a crosshair tooltip.
 */
export function dayAhead(values, start, label) {
  const [W, H, L, R, T, B] = [480, 110, 34, 8, 8, 18];
  const x = (i) => L + (i / (values.length - 1)) * (W - L - R);
  const y = (p) => T + (1 - p) * (H - T - B);
  const line = values.map((p, i) => (i ? "L" : "M") + x(i).toFixed(1) + " " + y(p).toFixed(1)).join("");
  const area = line + `L${x(values.length - 1)} ${y(0)}L${x(0)} ${y(0)}Z`;
  const grid = [0, 0.5, 1].map((p) =>
    s("g", null,
      s("line", { class: p === 0 ? "axis" : "grid", x1: L, x2: W - R, y1: y(p), y2: y(p) }),
      s("text", { class: "tick", x: L - 4, y: y(p) + 3, "text-anchor": "end" }, pct(p))));
  const ticks = values.map((_, i) => i).filter((i) => i % 6 === 0).map((i) =>
    s("text", { class: "tick", x: x(i), y: H - 4, "text-anchor": i === 0 ? "start" : "middle" },
      i === 0 ? "now" : hhmm(start + i * 3600).slice(0, 2) + "Z"));
  const cursor = s("g", { class: "cursor", visibility: "hidden" },
    s("line", { y1: T, y2: H - B }),
    s("circle", { r: 4 }));
  const svg = s("svg", { viewBox: `0 0 ${W} ${H}`, class: "chart", role: "img", "aria-label": label },
    grid, s("path", { class: "area s1", d: area }), s("path", { class: "line s1", d: line }), ticks, cursor);
  svg.addEventListener("pointermove", (e) => {
    const box = svg.getBoundingClientRect();
    const u = ((e.clientX - box.left) / box.width) * W;
    const i = Math.round(((u - L) / (W - L - R)) * (values.length - 1));
    if (i < 0 || i >= values.length) return hideTip();
    cursor.setAttribute("visibility", "visible");
    cursor.firstChild.setAttribute("x1", x(i));
    cursor.firstChild.setAttribute("x2", x(i));
    cursor.lastChild.setAttribute("cx", x(i));
    cursor.lastChild.setAttribute("cy", y(values[i]));
    showTip(e, [hhmm(start + i * 3600), pct(values[i]) + " " + label.toLowerCase()]);
  });
  svg.addEventListener("pointerleave", () => {
    cursor.setAttribute("visibility", "hidden");
    hideTip();
  });
  return svg;
}

/** The learned daily pattern: 24 cells, one per UTC hour, darker for likelier. */
export function dayPattern(values, label) {
  const [W, H, cell] = [480, 32, 480 / 24];
  const cells = values.map((p, hour) =>
    tipped(s("rect", { class: "cell " + seq(p), x: hour * cell + 1, y: 0, width: cell - 2, height: 16, rx: 2 }),
      () => [String(hour).padStart(2, "0") + ":00Z", pct(p) + " " + label.toLowerCase()]));
  const ticks = [0, 6, 12, 18].map((hour) =>
    s("text", { class: "tick", x: hour * cell + 1, y: H - 2 }, String(hour).padStart(2, "0") + "Z"));
  return s("svg", { viewBox: `0 0 ${W} ${H}`, class: "chart", role: "img", "aria-label": label }, cells, ticks);
}

// ---- A credible interval -----------------------------------------------------

/** A rate believed to be `i.mean`, 90 % within `i.low`–`i.high`: a band on a 0–100 % track. */
export function range(i, label) {
  const [W, H] = [160, 14];
  const x = (p) => 2 + p * (W - 4);
  const svg = s("svg", { viewBox: `0 0 ${W} ${H}`, class: "range", role: "img",
    "aria-label": `${label}: ${pct(i.mean)}, 90% between ${pct(i.low)} and ${pct(i.high)}` },
    s("line", { class: "track", x1: x(0), x2: x(1), y1: H / 2, y2: H / 2 }),
    s("rect", { class: "band", x: x(i.low), width: Math.max(2, x(i.high) - x(i.low)), y: 3, height: H - 6, rx: 3 }),
    s("line", { class: "mean", x1: x(i.mean), x2: x(i.mean), y1: 1, y2: H - 1 }));
  return tipped(svg, () => [label, pct(i.mean) + " expected", "90% between " + pct(i.low) + " and " + pct(i.high)]);
}

// ---- A chance moved by an observation ----------------------------------------

/** Where a chance was (hollow) and where one observation moved it (filled). */
export function shift(before, after) {
  const [W, H] = [96, 12];
  const x = (p) => 5 + p * (W - 10);
  const dir = after > before + 0.005 ? "up" : after < before - 0.005 ? "down" : "same";
  return s("svg", { viewBox: `0 0 ${W} ${H}`, class: "shift " + dir, role: "img", "aria-label": pct(before) + " to " + pct(after) },
    s("line", { class: "track", x1: x(0), x2: x(1), y1: H / 2, y2: H / 2 }),
    s("line", { class: "move", x1: x(before), x2: x(after), y1: H / 2, y2: H / 2 }),
    s("circle", { class: "from", cx: x(before), cy: H / 2, r: 3.5 }),
    s("circle", { class: "to", cx: x(after), cy: H / 2, r: 3.5 }));
}

// ---- Calibration --------------------------------------------------------------

/**
 * How the chances given came true: each band of the chance given (a dot,
 * its area the handoffs in it) against the share carried, the correction
 * applied (a line), and the diagonal where they would agree.
 */
export function reliability(view, label) {
  const [S, L, B, T, R] = [200, 30, 22, 6, 6];
  const x = (p) => L + p * (S - L - R);
  const y = (p) => T + (1 - p) * (S - T - B);
  const most = Math.max(1, ...view.bands.map((b) => b.outcomes));
  const grid = [0, 0.5, 1].map((p) => [
    s("line", { class: p === 0 ? "axis" : "grid", x1: x(0), x2: x(1), y1: y(p), y2: y(p) }),
    s("line", { class: p === 0 ? "axis" : "grid", x1: x(p), x2: x(p), y1: y(0), y2: y(1) }),
    s("text", { class: "tick", x: L - 4, y: y(p) + 3, "text-anchor": "end" }, pct(p)),
    s("text", { class: "tick", x: x(p), y: S - B + 13, "text-anchor": p === 0 ? "start" : p === 1 ? "end" : "middle" }, pct(p)),
  ]);
  const curve = view.curve.length > 1
    ? s("path", { class: "line s2", d: view.curve.map(([g, c], i) => (i ? "L" : "M") + x(g) + " " + y(c)).join("") })
    : null;
  const dots = view.bands.map((b) =>
    tipped(s("circle", { class: "dot s1", cx: x(b.given), cy: y(b.carried), r: 3 + 6 * Math.sqrt(b.outcomes / most) }),
      () => ["Given about " + pct(b.given), pct(b.carried) + " carried", count(b.outcomes) + " handoffs (faded)"]));
  return s("svg", { viewBox: `0 0 ${S} ${S}`, class: "chart square", role: "img", "aria-label": label },
    grid,
    s("line", { class: "diagonal", x1: x(0), y1: y(0), x2: x(1), y2: y(1) }),
    s("text", { class: "axis-label", x: x(0.5), y: S - 1, "text-anchor": "middle" }, "chance given"),
    curve, dots);
}

/** The legend the calibration charts share. */
export function reliabilityLegend() {
  return h("div", { class: "legend" },
    h("span", null, h("i", { class: "key dot s1" }), "handoffs, by chance given: share carried"),
    h("span", null, h("i", { class: "key line s2" }), "correction applied"),
    h("span", null, h("i", { class: "key diagonal" }), "given = carried"));
}
