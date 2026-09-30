// Beliefs: what the node makes of the channel, why each message goes or
// waits, the evidence as it comes in and what it moves, and how the chances
// the node gives have come true.

import { reliability, reliabilityLegend } from "../charts.js";
import { fill, h } from "../dom.js";
import { bearer, count, pct, span } from "../format.js";
import { decisionRow, evidenceRow, runs } from "../explain.js";
import { active } from "../messages.js";
import { poll, watchAll } from "../store.js";

const tile = (label, value, detail) =>
  h("div", { class: "tile" }, h("span", { class: "label" }, label), h("strong", null, value),
    detail ? h("span", { class: "muted small" }, detail) : null);

const FILTERS = { all: "All", link: "Paths", custodian: "Custodians", calibration: "Calibration" };

export function mount(root) {
  let filter = "all";
  let data = null;
  const tiles = h("div", { class: "tiles" });
  const waiting = h("ol", { class: "decisions" });
  const earlier = h("ol", { class: "decisions" });
  const earlierBox = h("details", null, h("summary", null, "Earlier decisions"), earlier);
  const filters = h("div", { class: "filters", role: "group", "aria-label": "Show evidence about" });
  const journal = h("ol", { class: "journal" });
  const charts = h("div", { class: "multiples" });
  root.replaceChildren(h("div", { class: "beliefs" },
    tiles,
    h("section", { class: "card" }, h("h2", null, "Messages and why"),
      h("p", { class: "muted small" }, "The latest routing decision about each message: the route it was planned on, each hop's chance, and what the route is worth once its airtime is paid for. A message waiting for a better moment is a decision too."),
      waiting, earlierBox),
    h("section", { class: "card" }, h("h2", null, "Evidence"),
      h("p", { class: "muted small" }, "Each observation as it was taken in, the last first, and how it moved the chance it bears on. The same observation of the same thing, again and again, is one row."),
      filters, journal),
    h("section", { class: "card" }, h("h2", null, "How the chances came true"),
      h("p", { class: "muted small" }, "Every handoff chance the node gives is checked against how the handoff ended. Where the dots leave the diagonal, the models err, and the correction (the line) is applied to later chances. Kept apart for paths seen open and paths only inferred."),
      reliabilityLegend(), charts)));

  function render() {
    if (!data) return;
    const [insight, outbox] = data;
    const at = insight.at;
    const known = insight.links.length;
    const seen = insight.links.filter((l) => l.seen).length;
    const pending = outbox.filter(active);
    const ch = insight.channel;
    fill(tiles,
      tile("Channel busy", pct(ch.busy), ch.radio_up ? "others on the air" : "radio down"),
      tile("Stations sharing it", (1 + ch.contenders).toFixed(1), "us included, expected"),
      tile("A minute on the air costs", (ch.airtime_price_per_min * 100).toFixed(1) + "%", "of a delivered message"),
      tile("Beacons every", span(ch.beacon_interval_secs), "longer as more share the channel"),
      tile("Stations known", String(insight.stations.length - 1)),
      tile("Paths believed in", String(known), seen + " seen open, " + (known - seen) + " inferred"),
      tile("Messages under way", String(pending.length)));

    const byId = new Map(outbox.map((m) => [m.id, m]));
    const current = insight.decisions.filter((d) => byId.has(d.id) && active(byId.get(d.id)));
    const past = insight.decisions.filter((d) => !current.includes(d));
    fill(waiting, current.length
      ? current.map((d) => decisionRow(d, at, byId.get(d.id)))
      : h("li", { class: "empty" }, "Nothing under way."));
    earlierBox.hidden = !past.length;
    fill(earlier, past.map((d) => decisionRow(d, at, byId.get(d.id))));

    fill(filters, Object.entries(FILTERS).map(([key, label]) =>
      h("button", { type: "button", "aria-pressed": String(filter === key), onclick: () => { filter = key; render(); } }, label)));
    const shown = runs(insight.journal.filter((u) => filter === "all" || u.subject === filter)).slice(0, 60);
    fill(journal, shown.length ? shown.map((u) => evidenceRow(u, at)) : h("li", { class: "empty" }, "No evidence yet."));

    const records = insight.calibration.filter((c) => c.outcomes > 0.5);
    fill(charts, records.length
      ? records.map((c) => h("figure", null,
        reliability(c, bearer(c.bearer).label + (c.seen ? ", paths seen open" : ", paths never seen open")),
        h("figcaption", null, h("strong", null, bearer(c.bearer).label), c.seen ? " · paths seen open" : " · paths never seen open",
          h("span", { class: "muted small" }, " · " + count(c.outcomes) + " handoffs"))))
      : h("p", { class: "empty" }, "No handoffs have ended yet."));
  }

  const stops = [
    watchAll(["insight", "outbox"], (insight, outbox) => {
      data = [insight, outbox];
      render();
    }),
    poll("insight", 20),
  ];
  return { unmount: () => stops.forEach((stop) => stop()) };
}
