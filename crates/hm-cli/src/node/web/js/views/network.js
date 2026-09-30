// Network: every station this one has heard of on a map, with the paths it
// believes in; a table of them; and what it believes of the one picked.

import { fill, h } from "../dom.js";
import { bearer, BEARERS, compass, km, pct, relative } from "../format.js";
import { distance } from "../geo.js";
import { NetworkMap } from "../map.js";
import { poll, store, watchAll } from "../store.js";
import { keyPill, stationPanel } from "./station.js";

/** The path from this station to `call` likeliest to carry a handoff now, or null. */
function bestPath(insight, call) {
  const mine = insight.links.filter((l) => l.mine && (l.a === call || l.b === call));
  return mine.length ? mine.reduce((a, b) => (b.success > a.success ? b : a)) : null;
}

function legend() {
  return h("div", { class: "legend" },
    Object.entries(BEARERS).map(([id, b]) => h("span", null, h("i", { class: "key path b-" + id }), b.label)),
    h("span", null, h("i", { class: "key path thin" }), h("i", { class: "key path thick" }), "wider: likelier a handoff completes now"),
    h("span", null, h("i", { class: "key path inferred" }), "dashed: never seen open"));
}

function bearersCard(status) {
  const rows = [];
  const add = (id, text) => rows.push(h("dt", { title: bearer(id).tip }, bearer(id).label), h("dd", null, text));
  add("radio", status.radio === null ? "not configured" : (status.radio ? "up" : "down") + (status.radio_via ? " · " + status.radio_via : ""));
  add("internet", status.internet_listen || status.internet_peers.length
    ? [status.internet_listen ? "listening on " + status.internet_listen : null,
      status.internet_peers.length ? "linked to " + status.internet_peers.join(", ") : "no peers linked"].filter(Boolean).join(" · ")
    : "not configured");
  add("modem", status.modem === null ? "none" : (status.modem ? "ready" : "down") +
    (status.modem_peer ? " · linked to " + status.modem_peer : "") + (status.modem_via ? " · " + status.modem_via : ""));
  return h("dl", { class: "facts" }, rows);
}

export function mount(root, arg) {
  let selected = arg ? decodeURIComponent(arg).toUpperCase() : null;
  let insight = null;
  const mapBox = h("div", { class: "map-box" });
  const unplaced = h("p", { class: "muted small" });
  const table = h("tbody");
  const panel = h("div", { class: "card detail", "aria-live": "polite" });
  const bearers = h("div");
  root.replaceChildren(h("div", { class: "network" },
    h("div", { class: "card wide map-card" }, h("div", { class: "card-head" }, h("h2", null, "Stations and paths"), legend()), mapBox, unplaced),
    h("div", { class: "column" },
      h("div", { class: "card" }, h("h2", null, "Stations"),
        h("div", { class: "table-scroll" }, h("table", { class: "data" },
          h("thead", null, h("tr", null, ["Station", "Where", "Heard", "Best path now", "Next open"].map((t) => h("th", null, t)))), table))),
      h("div", { class: "card" }, h("h2", null, "Bearers"), bearers)),
    panel));

  const map = new NetworkMap(mapBox, (call) => (location.hash = "#network/" + encodeURIComponent(call)));

  function render() {
    if (!insight) return;
    const me = insight.stations.find((st) => st.me);
    map.show(insight, selected);
    const missing = insight.stations.filter((st) => !st.place).length;
    unplaced.textContent = missing ? missing + " of " + insight.stations.length + " stations give no locator: they are in the table, not on the map." : "";
    const rows = insight.stations
      .map((st) => ({ st, best: st.me ? null : bestPath(insight, st.call) }))
      .sort((a, b) => Number(b.st.me) - Number(a.st.me) || (b.best?.success ?? -1) - (a.best?.success ?? -1) || (b.st.heard_at ?? 0) - (a.st.heard_at ?? 0));
    fill(table, rows.map(({ st, best }) => {
      const far = me?.place && st.place && !st.me ? distance(me.place, st.place) : null;
      return h("tr", { class: st.call === selected ? "selected" : null, tabindex: 0,
        onclick: () => (location.hash = "#network/" + encodeURIComponent(st.call)),
        onkeydown: (e) => e.key === "Enter" && (location.hash = "#network/" + encodeURIComponent(st.call)) },
      h("td", null, h("strong", null, st.call), st.me ? h("span", { class: "muted small" }, " you") : null, " ", keyPill(st.beacon?.key)),
      h("td", { class: "num" }, far ? km(far.km) + " " + compass(far.bearing) : st.place ? st.place.locator : "–"),
      h("td", { class: "num" }, st.me ? "" : st.heard_at ? relative(st.heard_at, insight.at) : "not heard"),
      h("td", { class: "num" }, best ? pct(best.success) + " " + bearer(best.bearer).short : st.me ? "" : "none"),
      h("td", { class: "num" }, best ? (best.open_now >= 0.5 ? "open" : best.next_opening ? relative(best.next_opening, insight.at) : "not within a day") : ""));
    }));
    fill(panel, selected
      ? stationPanel(insight, selected)
      : h("div", null, h("h2", null, "Pick a station"),
        h("p", { class: "muted" }, "Pick one on the map or in the table to see what this station believes of it: its paths, their day ahead, how lossy they are, how it does as a custodian, and the evidence behind it all.")));
  }

  const stops = [
    watchAll(["insight"], (value) => {
      insight = value;
      render();
    }),
    store.watch("status", (status) => fill(bearers, bearersCard(status))),
    poll("insight", 20),
  ];

  return {
    update(next) {
      selected = next ? decodeURIComponent(next).toUpperCase() : null;
      render();
      if (selected) map.focus(selected);
    },
    unmount() {
      stops.forEach((stop) => stop());
      map.destroy();
    },
  };
}
