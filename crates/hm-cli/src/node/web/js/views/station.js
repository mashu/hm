// One station as this one knows it: where it is and what its beacon says,
// what is believed of every path to it and of it as a custodian, the latest
// evidence about it, and the messages routed to it.

import { dayAhead, dayPattern, range } from "../charts.js";
import { h } from "../dom.js";
import { bearer, compass, count, hhmm, KEYS, km, pct, relative, span } from "../format.js";
import { distance } from "../geo.js";
import { decisionRow, evidenceRow, runs } from "../explain.js";

/** A labelled figure: "Open now / 63%". */
const figure = (label, value, detail, title) =>
  h("div", { class: "figure", title }, h("span", { class: "label" }, label), h("strong", null, value),
    detail ? h("span", { class: "muted small" }, detail) : null);

export function keyPill(key) {
  if (!key) return null;
  const [text, tone] = KEYS[key];
  const icon = { trusted: "✓", unknown: "?", mismatch: "!" }[key];
  return h("span", { class: ["pill", tone], title: text }, icon + " " + { trusted: "key trusted", unknown: "key unknown", mismatch: "key mismatch" }[key]);
}

/** What is believed of one path, with its day ahead and its evidence. */
export function pathCard(link, at, from) {
  const other = link.a === from ? link.b : link.a;
  const [lost, got] = link.frames;
  const [done, failed] = link.handoffs;
  return h("section", { class: "path-card" },
    h("header", null,
      h("h4", null, link.mine ? other : link.a + " – " + link.b),
      h("span", { class: ["pill", "b-" + link.bearer], title: bearer(link.bearer).tip }, bearer(link.bearer).label),
      link.seen ? null : h("span", { class: "pill", title: "Never seen open: believed from the population of paths like it, and from what was missed" }, "inferred")),
    h("div", { class: "figures" },
      figure("Handoff now", pct(link.success), "model: " + pct(link.success_model),
        "The path model gives " + pct(link.success_model) + "; the record of how the chances given came true corrects it to " + pct(link.success)),
      figure("Open now", pct(link.open_now)),
      figure("Within reach", pct(link.reach)),
      figure("Next likely open", link.open_now >= 0.5 ? "now" : link.next_opening ? hhmm(link.next_opening) : "not within a day",
        link.next_opening && link.open_now < 0.5 ? relative(link.next_opening, at) : null)),
    h("div", { class: "chart-block" },
      h("span", { class: "label" }, "Chance open, next 24 hours"), dayAhead(link.forecast, at, "Chance open")),
    h("div", { class: "chart-block" },
      h("span", { class: "label" }, "Learned daily pattern (UTC), were it within reach"), dayPattern(link.daily, "Chance open")),
    h("dl", { class: "facts" },
      h("dt", null, "Frame loss while open"), h("dd", null, range(link.frame_loss, "Frame loss"), " ", pct(link.frame_loss.mean)),
      h("dt", null, "Handoff completes when open"), h("dd", null, range(link.handoff_if_open, "Handoff completes when open"), " ", pct(link.handoff_if_open.mean)),
      h("dt", null, "Openings last about"), h("dd", null, span(link.persistence_mins * 60)),
      h("dt", null, "Last seen open"), h("dd", null, link.last_open ? relative(link.last_open, at) : "never"),
      link.beacon_interval ? [h("dt", null, "Beacons every"), h("dd", null, span(link.beacon_interval))] : null,
      h("dt", null, "Evidence (faded)"), h("dd", null,
        count(got) + " frames arrived, " + count(lost) + " lost · " + count(done) + " handoffs completed, " + count(failed) + " failed")));
}

/** The panel for `call`. */
export function stationPanel(insight, call) {
  const st = insight.stations.find((x) => x.call === call);
  if (!st) return h("p", { class: "empty" }, call + " is not known here.");
  const me = insight.stations.find((x) => x.me);
  const at = insight.at;
  const far = me?.place && st.place && !st.me ? distance(me.place, st.place) : null;
  const facts = [
    st.place ? st.place.locator : "no locator",
    far ? km(far.km) + " " + compass(far.bearing) : null,
    st.me ? null : st.heard_at ? "heard " + relative(st.heard_at, at) : "not heard here",
    st.beacon && st.beacon.clock_offset !== 0 ? "clock " + (st.beacon.clock_offset > 0 ? "+" : "") + st.beacon.clock_offset + " s" : null,
    st.offers.length ? "offers " + st.offers.join(", ") : null,
    st.linked.length ? "in contact now by " + st.linked.map((b) => bearer(b).short).join(", ") : null,
  ].filter(Boolean);
  const links = insight.links.filter((l) => (st.me ? l.mine : l.a === call || l.b === call))
    .sort((a, b) => Number(b.mine) - Number(a.mine) || b.success - a.success);
  const custodian = insight.custodians.find((c) => c.call === call);
  const evidence = runs(insight.journal.filter((u) => u.stations.includes(call))).slice(0, 12);
  const decisions = insight.decisions
    .filter((d) => d.to === call || (d.verdict !== "hold" && d.route.hops.some((hop) => hop.to === call)))
    .slice(0, 8);
  return h("div", { class: "station-panel" },
    h("header", null,
      h("h3", null, call),
      st.me ? h("span", { class: "pill" }, "this station") : null,
      st.trusted && !st.beacon ? h("span", { class: "pill ok", title: "Its key is among the trusted" }, "✓ trusted") : null,
      keyPill(st.beacon?.key),
      st.me ? null : h("a", { class: "btn quiet small", href: "#chat/" + encodeURIComponent(call) }, "Chat")),
    h("p", { class: "muted" }, facts.join(" · ")),
    st.beacon?.key === "mismatch" ? h("p", { class: "note bad" }, "! " + KEYS.mismatch[0] + ". Its beacons are not believed.") : null,
    st.beacon?.hears.length ? h("p", { class: "small" }, "Its beacon says it hears ",
      st.beacon.hears.map(([c, mins], i) => [i ? ", " : "", h("a", { href: "#network/" + encodeURIComponent(c) }, c), " (" + span(mins * 60) + " ago)"])) : null,
    h("h4", { class: "eyebrow" }, st.me ? "Paths from here" : "Paths"),
    links.length ? links.map((l) => pathCard(l, at, st.me ? call : me?.call)) : h("p", { class: "empty" }, "No path to it is believed in yet."),
    custodian ? [
      h("h4", { class: "eyebrow" }, "As a custodian"),
      h("dl", { class: "facts" },
        h("dt", null, "Takes custody"), h("dd", null, range(custodian.accepts, "Takes custody"), " ", pct(custodian.accepts.mean)),
        h("dt", null, "Does its part"), h("dd", null, range(custodian.delivers, "Does its part"), " ", pct(custodian.delivers.mean)),
        h("dt", null, "Receipts late by"), h("dd", null, span(custodian.lateness[0]) + " typically, " + span(custodian.lateness[1]) + " at the 90th percentile"),
        custodian.busy_until ? [h("dt", null, "Busy"), h("dd", null, "until " + hhmm(custodian.busy_until))] : null,
        h("dt", null, "Evidence (faded)"), h("dd", null,
          count(custodian.accept_counts[0]) + " taken, " + count(custodian.accept_counts[1]) + " refused · " +
          count(custodian.deliver_counts[0]) + " delivered, " + count(custodian.deliver_counts[1]) + " silent")),
    ] : null,
    evidence.length ? [h("h4", { class: "eyebrow" }, "Latest evidence"), h("ol", { class: "journal" }, evidence.map((u) => evidenceRow(u, at)))] : null,
    decisions.length ? [h("h4", { class: "eyebrow" }, "Latest messages routed to or through it"), h("ol", { class: "decisions" }, decisions.map((d) => decisionRow(d, at)))] : null);
}
