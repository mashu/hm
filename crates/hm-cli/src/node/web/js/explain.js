// The node's reasons, in words: one piece of evidence and what it did to a
// belief; one routing decision and the route it was made on.

import { shift } from "./charts.js";
import { h } from "./dom.js";
import { bearer, hhmm, pct, relative, span } from "./format.js";

const SUBJECTS = {
  link: (u) => u.stations.join(" – ") + " · " + bearer(u.bearer).short,
  custodian: (u) => u.stations[0] + " as custodian",
  calibration: (u) => bearer(u.bearer).label + " record",
};

const MEANS = {
  link: "chance a handoff over it completes now",
  custodian: "chance it takes custody and does its part",
  calibration: "chance that comes true when that chance is given",
};

/**
 * The same observation of the same subject again and again (a beacon due
 * and missed every ten minutes) as one run: how many, over how long, and
 * how the chance moved over them all. Only another observation of that
 * subject ends a run. `updates` and the runs: the last taken in first.
 */
export function runs(updates) {
  const open = new Map();
  const all = [];
  [...updates].reverse().forEach((u, taken) => {
    const subject = [u.subject, u.bearer, ...u.stations].join(" ");
    const run = open.get(subject);
    if (run && run.last.observed === u.observed) {
      run.count++;
      run.last = u;
      run.taken = taken;
    } else {
      const fresh = { first: u, last: u, count: 1, taken };
      open.set(subject, fresh);
      all.push(fresh);
    }
  });
  return all.sort((a, b) => b.taken - a.taken);
}

/** One run of observations: when, about what, what was seen, and the chance before and after. */
export function evidenceRow(run, at) {
  const { first, last, count } = run;
  return h("li", { class: "evidence" },
    h("span", { class: "when", title: hhmm(last.at) }, relative(last.at, at)),
    h("span", { class: "about" }, SUBJECTS[last.subject](last)),
    h("span", { class: "what" }, last.observed,
      count > 1 ? h("span", { class: "muted" }, " ×" + count + " over " + span(last.at - first.at)) : null),
    h("span", { class: "moved", title: MEANS[last.subject] }, shift(first.before, last.after), " ", pct(first.before) + " → " + pct(last.after)));
}

const VERDICTS = {
  send: ["↗", "Sent", "Handed to the first hop now"],
  wait: ["◷", "Waits", "Waits for the route's first departure"],
  hear: ["◉", "Listens", "Waits to hear the first hop, then goes"],
  hold: ["‖", "Held", "No way there worth its airtime now"],
};

/** A route as a chain of hops, each with its chance. */
export function routeChain(route) {
  const parts = [h("strong", null, route.hops[0].from)];
  for (const hop of route.hops) {
    parts.push(h("span", { class: "hop", title: bearer(hop.bearer).label + ", leaves " + hhmm(hop.depart) },
      " → " + bearer(hop.bearer).short + " " + pct(hop.chance) + " → "), h("strong", null, hop.to));
  }
  return h("span", { class: "route" }, parts);
}

/** The latest decision about a message. */
export function decisionRow(d, at, message) {
  const [icon, label, meaning] = VERDICTS[d.verdict];
  const route = d.route;
  return h("li", { class: "decision" },
    h("span", { class: ["pill", "verdict-" + d.verdict], title: meaning }, icon + " " + label),
    h("span", { class: "to" }, h("strong", null, d.to), message?.subject ? " · " + message.subject : message?.text ? " · " + message.text.slice(0, 48) : ""),
    h("span", { class: "muted small" }, "decided " + relative(d.at, at)),
    route
      ? h("span", { class: "why" }, routeChain(route),
        h("span", { class: "muted small" },
          " · delivers with " + pct(route.chance) +
          (d.verdict === "send" ? "" : ", first hop " + relative(route.hops[0].depart, at)) +
          " · arrives about " + hhmm(route.arrival) +
          " · worth " + route.utility.toFixed(2) + " of a delivered message after its cost"))
      : h("span", { class: "why muted" }, d.reason));
}
