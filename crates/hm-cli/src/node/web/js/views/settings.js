// Settings: every setting, described once below, saved with one PATCH; the
// grid square picked on a map; the trusted stations.

import { api } from "../api.js";
import { action, fill, h, note } from "../dom.js";
import { Form } from "../form.js";
import { formatLocator, fuzz, toLocator } from "../geo.js";
import { LocatorPicker } from "../map.js";
import { store, watchAll } from "../store.js";

const secs = { type: "number", min: 1, required: true };

/** Every panel and its fields; paths are those of `GET /api/settings` (`live`, and `restart_to_change`). */
const PANELS = [
  { id: "station", title: "Station", nav: "Your grid square",
    blurb: "Your grid square goes out in beacons, so others see how far you are and in which direction." },
  { id: "radio", title: "Packet radio", nav: "TNC or sound card, transmitter",
    blurb: "A KISS TNC (Direwolf, a hardware TNC) or the built-in AFSK modem. Changes open a new radio link at once.",
    groups: [
      { fields: [{ path: "radio.enabled", type: "check", label: "Use packet radio", hint: "Off for an internet-only node" }] },
      { title: "Interface", when: "radio.enabled", fields: [
        { path: "radio.kiss", label: "KISS TNC", placeholder: "127.0.0.1:8001 or serial:/dev/ttyUSB0:9600" },
        { path: "radio.audio", label: "Sound card", hint: "empty: use the TNC above", placeholder: "default" },
        { path: "radio.ptt", type: "choice", label: "PTT", options: [["vox", "VOX / none"], ["rigctld", "rigctld (Hamlib)"]], placeholder: "rts:/dev/ttyUSB0, cm108:/dev/hidraw0" },
        { path: "radio.framing", type: "select", label: "Framing", options: [["ax25", "AX.25: widest compatibility"], ["auto", "Auto: IL2P to stations that decode it"], ["il2p", "IL2P: more robust in noise"]] },
        { path: "beacon_minutes", type: "number", min: 0, required: true, label: "Beacon every", hint: "minutes; 0: none" },
      ] },
      { title: "Transmitter", when: "radio.enabled", columns: 2,
        lede: "Data modes keep the transmitter at full output while keyed. Keep within what your transceiver is rated for.",
        fields: [
          { path: "radio.max_keyup_secs", type: "number", min: 0, required: true, label: "Longest key-up", hint: "s; 0: no limit" },
          { path: "radio.duty_cycle_percent", type: "number", min: 1, max: 100, required: true, label: "Duty cycle", hint: "% of the time on the air, long-run" },
          { path: "radio.txdelay_ms", type: "number", min: 0, step: 10, required: true, label: "Key-up delay", hint: "ms" },
          { path: "radio.tnc_port", type: "number", min: 0, max: 15, required: true, label: "TNC port" },
        ] },
      { title: "Channel access", hint: "CSMA, bit rate, handoff rounds", advanced: true, when: "radio.enabled", columns: 2, fields: [
        { path: "radio.persist", type: "number", min: 0, max: 255, required: true, label: "Persistence", hint: "0–255" },
        { path: "radio.slottime_ms", type: "number", min: 0, step: 10, required: true, label: "Slot time", hint: "ms" },
        { path: "radio.bitrate", type: "number", min: 1, required: true, label: "Bit rate", hint: "bd" },
        { path: "radio.guard_ms", type: "number", min: 0, step: 10, required: true, label: "Guard", hint: "ms" },
        { path: "radio.max_rounds", type: "number", min: 1, required: true, label: "Rounds without progress", hint: "before a handoff fails" },
      ] },
    ] },
  { id: "internet", title: "Internet", nav: "Peers and listening",
    blurb: "Peers are dialled at once. Listening and open hub take a restart.",
    groups: [
      { title: "Listening", lede: "Accept QUIC links from other stations. Used from the next start.", fields: [
        { path: "restart_to_change.internet_listen", label: "Address", hint: "empty: do not listen", placeholder: "0.0.0.0:4433" },
        { path: "restart_to_change.open_hub", type: "check", label: "Open hub", hint: "Accept links from any station with a valid certificate" },
      ] },
    ] },
  { id: "modem", title: "ARQ modem", nav: "VARA, ARDOP, Mercury",
    blurb: "An external ARQ modem program, beside packet radio. Used from the next start.",
    groups: [
      { fields: [{ path: "restart_to_change.modem.enabled", type: "check", label: "Use an ARQ modem" }] },
      { when: "restart_to_change.modem.enabled", columns: 2, fields: [
        { path: "restart_to_change.modem.kind", type: "select", label: "Kind", options: [["vara", "VARA / Mercury"], ["ardop", "ARDOP"]] },
        { path: "restart_to_change.modem.host", label: "Host", required: true, placeholder: "127.0.0.1" },
        { path: "restart_to_change.modem.port", type: "number", min: 1, max: 65534, required: true, label: "Command port" },
        { path: "restart_to_change.modem.bandwidth", type: "choice", numeric: true, label: "Bandwidth", hint: "Hz",
          options: [["0", "Modem default"], ["500", "500 Hz"], ["2300", "2300 Hz"], ["2750", "2750 Hz"]], placeholder: "Hz" },
        { path: "restart_to_change.modem.ptt", type: "choice", label: "PTT", options: [["none", "None: the modem keys the radio"], ["vox", "VOX"], ["rigctld", "rigctld"]], placeholder: "rts:/dev/ttyUSB0" },
      ] },
    ] },
  { id: "delivery", title: "Delivery", nav: "Costs, retries, custody",
    blurb: "Each message goes the way, now or later, worth most for its chance and speed after what sending costs.",
    groups: [
      { title: "What sending costs", lede: "In hundredths of a delivered message's value.", columns: 3, fields: [
        { path: "radio_cost", type: "number", min: 0.01, step: 0.01, required: true, label: "Radio", hint: "a minute on the air" },
        { path: "internet_cost", type: "number", min: 0.01, step: 0.01, required: true, label: "Internet", hint: "an attempt" },
        { path: "modem_cost", type: "number", min: 0.01, step: 0.01, required: true, label: "ARQ modem", hint: "a minute on the air" },
      ] },
      { title: "Retries", columns: 3, fields: [
        { path: "retry_first_secs", ...secs, label: "First retry", hint: "s" },
        { path: "retry_max_secs", ...secs, label: "Longest wait", hint: "s" },
        { path: "retry_attempts", ...secs, label: "Attempts" },
      ] },
      { title: "Custody and receipts", hint: "copies kept, reclaiming", advanced: true, columns: 3, fields: [
        { path: "custody_grace_secs", ...secs, label: "Keep a copy for", hint: "s after handing on" },
        { path: "custody_suspect_secs", ...secs, label: "Reclaim after at most", hint: "s without a receipt" },
        { path: "receipt_retry_attempts", ...secs, label: "Receipt attempts" },
      ] },
    ] },
  { id: "relay", title: "Relay", nav: "Multi-hop and mailbox",
    blurb: "Carry traffic for other stations, or hold it until they check in.",
    groups: [
      { fields: [
        { path: "relay.enabled", type: "check", label: "Relay", hint: "Take custody of traffic for other stations" },
        { path: "relay.mailbox", type: "check", label: "Mailbox", hint: "Hold mail for stations that call in now and then" },
      ] },
      { title: "Limits", hint: "storage, hops, airtime", advanced: true, columns: 2, fields: [
        { path: "relay.max_holdings", ...secs, label: "Most messages held" },
        { path: "relay.max_bytes", ...secs, label: "Most bytes held" },
        { path: "relay.max_hops", type: "number", min: 1, max: 16, required: true, label: "Most hops" },
        { path: "relay.airtime_budget_secs", ...secs, label: "Airtime per message", hint: "s" },
        { path: "relay.control_airtime_fraction", type: "number", min: 0, max: 1, step: 0.001, required: true, label: "Share of airtime for control" },
      ] },
    ] },
  { id: "trust", title: "Trusted stations", nav: "Whose keys you know",
    blurb: "Messages from trusted stations are verified; beacons naming another key are flagged. Changes apply at once." },
  { id: "node", title: "This node", nav: "Web page and store",
    blurb: "Where the web page listens and where messages are kept. Used from the next start.",
    groups: [{ fields: [
      { path: "restart_to_change.http", label: "Web address", required: true, placeholder: "127.0.0.1:8080" },
      { path: "restart_to_change.store", label: "Store", required: true, placeholder: "station.db" },
    ] }] },
];

/** The station panel: a locator typed, taken from the device, or picked on the map, within a privacy range. */
function locatorPanel(onChange) {
  let anchor = null;
  let picker = null;
  const input = h("input", { id: "locator", maxlength: 6, spellcheck: false, placeholder: "JO89xi", autocomplete: "off" });
  const rangeKm = h("select", { id: "locator-range" },
    [["5", "± 5 km"], ["10", "± 10 km"], ["25", "± 25 km"], ["50", "± 50 km"], ["0", "Exact"]].map(([v, l]) => h("option", { value: v, selected: v === "10" }, l)));
  const hint = h("p", { class: "note", role: "status" });
  const again = h("button", { type: "button", class: "btn quiet", hidden: true }, "Pick another square");
  const box = h("div", { class: "map-box small" });
  const km = () => Number(rangeKm.value) || 0;
  const show = (pan) => picker?.show(formatLocator(input.value), anchor, km(), pan);
  const choose = (place, how) => {
    anchor = place;
    const picked = fuzz(place, km());
    const loc = toLocator(picked.lat, picked.lon);
    input.value = loc;
    again.hidden = km() <= 0;
    note(hint, km() > 0 ? `${loc}: a square picked at random within ${km()} km of ${how}.` : `${loc}, exactly where ${how} is.`, "ok");
    show(true);
    onChange();
  };
  input.addEventListener("input", () => {
    anchor = null;
    again.hidden = true;
    const loc = formatLocator(input.value);
    if (!input.value.trim()) note(hint, "Not sent in beacons.");
    else if (loc) note(hint, loc);
    else note(hint, "Not a Maidenhead locator (like JO89 or JO89xi).", "bad");
    show(!!loc);
  });
  rangeKm.addEventListener("change", () => (anchor ? choose(anchor, "the same place") : show(false)));
  again.addEventListener("click", () => anchor && choose(anchor, "the same place"));
  const gps = h("button", { type: "button", class: "btn quiet" }, "Use my location");
  gps.addEventListener("click", () => {
    if (!navigator.geolocation) return note(hint, "This browser cannot give a location.", "bad");
    note(hint, "Finding your location…");
    navigator.geolocation.getCurrentPosition(
      (p) => choose({ lat: p.coords.latitude, lon: p.coords.longitude }, "your location"),
      (e) => note(hint, ["", "Permission denied", "Location unavailable", "Timed out"][e.code] || "No location", "bad"),
      { enableHighAccuracy: true, timeout: 15000, maximumAge: 60000 });
  });
  const node = h("div", null,
    h("label", { htmlFor: "locator" }, "Grid locator", h("span", { class: "muted" }, " (empty: not sent)")),
    h("div", { class: "row" }, input, gps),
    h("div", { class: "row" }, h("label", { htmlFor: "locator-range" }, "Privacy range"), rangeKm, again),
    box, hint);
  return {
    node,
    mount: () => {
      picker = new LocatorPicker(box, (place) => choose(place, "the point clicked"));
      show(true);
    },
    unmount: () => picker?.destroy(),
    set: (loc) => {
      input.value = loc || "";
      anchor = null;
      again.hidden = true;
      note(hint, loc ? loc : "Click the map, use your location, or type a square.");
      show(true);
    },
    get: () => formatLocator(input.value) || "",
    problem: () => (input.value.trim() && !formatLocator(input.value) ? [input, "Not a Maidenhead locator (like JO89 or JO89xi)"] : null),
  };
}

/** The internet peers dialled, one row each. */
function peersEditor(onChange) {
  const list = h("div", { class: "peers" });
  const row = (p) => {
    const call = h("input", { value: p.station, placeholder: "SO5KM", maxlength: 9, spellcheck: false, class: "call", "aria-label": "Peer callsign" });
    const address = h("input", { value: p.address, placeholder: "hub.example.org:4433", spellcheck: false, "aria-label": "Peer address (host:port)" });
    const node = h("div", { class: "peer" }, call, address,
      h("button", { type: "button", class: "btn quiet small", onclick: () => { node.remove(); onChange(); } }, "Remove"));
    return node;
  };
  return {
    node: h("div", { class: "group" }, h("h3", { class: "eyebrow" }, "Peers"),
      h("p", { class: "muted small" }, "Stations this node keeps a link to."), list,
      h("button", { type: "button", class: "btn quiet small", onclick: () => { list.append(row({ station: "", address: "" })); onChange(); } }, "Add a peer")),
    set: (peers) => fill(list, (peers || []).map(row)),
    get: () => [...list.querySelectorAll(".peer")]
      .map((r) => r.querySelectorAll("input"))
      .map(([c, a]) => ({ station: c.value.trim().toUpperCase(), address: a.value.trim() }))
      .filter((p) => p.station || p.address),
    problem: () => {
      for (const r of list.querySelectorAll(".peer")) {
        const [c, a] = r.querySelectorAll("input");
        if ((c.value.trim() || a.value.trim()) && (!c.value.trim() || !a.value.includes(":"))) return [a, "Each peer needs a callsign and host:port"];
      }
      return null;
    },
  };
}

/** The trusted stations: listed, added from the line `hm whoami` prints, removed. */
function trustPanel() {
  const list = h("ul", { class: "list" });
  const line = h("input", { id: "trust-line", required: true, spellcheck: false, placeholder: "SO5KM-1 8a1e…" });
  const who = h("input", { id: "trust-who", maxlength: 80 });
  const result = h("p", { class: "note", role: "status" });
  const mine = h("code", { class: "wrap" });
  const form = h("form", { autocomplete: "off" },
    h("label", { htmlFor: "trust-line" }, "Add a station", h("span", { class: "muted" }, " (the line hm whoami prints on their side)")), line,
    h("label", { htmlFor: "trust-who" }, "Note", h("span", { class: "muted" }, " (optional, like a name)")), who,
    h("button", { class: "btn", type: "submit" }, "Trust this station"), result);
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    try {
      const s = await api.post("/api/trust", { line: line.value.trim(), note: who.value.trim() || undefined });
      note(result, s.station + " is trusted now.", "ok");
      line.value = who.value = "";
      store.changed("settings");
    } catch (error) {
      note(result, "Not added: " + error.message, "bad");
    }
  });
  return {
    node: h("div", null, list, form,
      h("div", { class: "group" }, h("h3", { class: "eyebrow" }, "Your line, for other stations"), mine,
        action("Copy", async (b) => {
          await navigator.clipboard.writeText(mine.textContent);
          b.textContent = "Copied";
        }, { class: "btn quiet small" }))),
    render(trusted, status) {
      mine.textContent = status.trust_line;
      fill(list, trusted.stations.length ? trusted.stations.map((s) => h("li", null,
        h("span", { class: "grow" }, h("strong", null, s.station), s.note ? h("span", { class: "muted" }, " · " + s.note) : null,
          h("span", { class: "key" }, s.key)),
        h("a", { class: "btn quiet small", href: "#chat/" + encodeURIComponent(s.station) }, "Chat"),
        action("Remove", async () => {
          if (!confirm("Stop trusting " + s.station + "?")) return;
          await api.del("/api/trust/" + encodeURIComponent(s.station));
          store.changed("settings");
        }, { class: "btn quiet small", "aria-label": "Stop trusting " + s.station })))
        : h("li", { class: "empty" }, "Nobody yet. Messages from stations not trusted arrive marked unverified."));
      if (!trusted.file) note(result, "This node has no settings file: trust lasts until it restarts.");
    },
  };
}

export function mount(root, arg) {
  let dirty = false;
  let settings = null;
  const touched = () => {
    dirty = true;
  };
  const locator = locatorPanel(touched);
  const peers = peersEditor(touched);
  const trust = trustPanel();
  const forms = new Map(PANELS.filter((p) => p.groups).map((p) => [p.id, new Form(p.groups)]));
  const saveNote = h("p", { class: "note", role: "status" });
  const fileNote = h("span", { class: "muted small" });
  const nav = h("nav", { class: "settings-nav", role: "tablist", "aria-label": "Settings sections" });
  const panels = PANELS.map((p) => {
    const body = [
      p.id === "station" ? locator.node : null,
      p.id === "internet" ? peers.node : null,
      p.id === "trust" ? trust.node : null,
      forms.get(p.id)?.elements,
    ];
    return h("section", { class: "card panel", role: "tabpanel", dataset: { panel: p.id } },
      h("header", null, h("h2", null, p.title), h("p", { class: "muted" }, p.blurb)), body);
  });
  const footer = h("div", { class: "save-bar" }, h("button", { class: "btn", type: "button", onclick: save }, "Save settings"), fileNote, saveNote);
  // Not a <form>: the trusted stations panel has its own.
  const page = h("div", { class: "settings", oninput: touched }, nav, h("div", { class: "panels" }, panels, footer));
  root.replaceChildren(page);

  function showPanel(id) {
    const panel = PANELS.find((p) => p.id === id) || PANELS[0];
    for (const b of nav.children) b.setAttribute("aria-selected", String(b.dataset.panel === panel.id));
    for (const p of panels) p.hidden = p.dataset.panel !== panel.id;
    footer.hidden = panel.id === "trust";
    if (location.hash !== "#settings/" + panel.id) history.replaceState(null, "", "#settings/" + panel.id);
  }
  fill(nav, PANELS.map((p) => h("button", { type: "button", role: "tab", dataset: { panel: p.id }, onclick: () => showPanel(p.id) },
    p.title, h("span", { class: "nav-label" }, p.nav))));

  function show(v) {
    settings = v;
    dirty = false;
    const flat = { ...v.live, restart_to_change: v.restart_to_change };
    for (const f of forms.values()) f.fill(flat);
    locator.set(v.live.locator);
    peers.set(v.live.peers);
    fill(fileNote, [
      v.file ? "Saved to " + v.file + "." : "No settings file: changes last until restart.",
      v.overridden?.length ? " Set on the command line for this run: " + v.overridden.join(", ") + "." : "",
    ].join(""));
  }

  function fail(input, message) {
    const panel = input.closest("[data-panel]");
    if (panel) showPanel(panel.dataset.panel);
    input.setCustomValidity(message);
    input.reportValidity();
    input.setCustomValidity("");
    note(saveNote, message, "bad");
  }

  async function save() {
    for (const f of forms.values()) {
      const bad = f.problem();
      if (bad) return fail(...bad);
    }
    const bad = locator.problem() || peers.problem();
    if (bad) return fail(...bad);
    const body = { locator: locator.get(), peers: peers.get() };
    for (const f of forms.values()) f.collect(body);
    if (body.retry_max_secs < body.retry_first_secs) {
      return fail(forms.get("delivery").input("retry_max_secs"), "The longest wait must be at least the first retry");
    }
    if (body.radio?.enabled && !body.radio.kiss && !body.radio.audio) {
      return fail(forms.get("radio").input("radio.kiss"), "Give a KISS TNC or a sound card");
    }
    try {
      show(await api.patch("/api/settings", body));
      note(saveNote, "Saved. Settings marked for the next start take a restart; the rest are in use now.", "ok");
    } catch (error) {
      note(saveNote, "Not saved: " + error.message, "bad");
    }
  }

  showPanel(arg || "station");
  locator.mount();
  const stops = [
    store.watch("settings", (v) => {
      if (!dirty || !settings) return show(v);
      note(saveNote, "The settings were changed elsewhere. Saving keeps yours; ", "warn");
      saveNote.append(h("button", { type: "button", class: "link", onclick: () => show(v) }, "show theirs instead"));
    }),
    watchAll(["trust", "status"], (t, st) => trust.render(t, st)),
  ];
  return {
    update: (next) => showPanel(next || "station"),
    unmount() {
      stops.forEach((stop) => stop());
      locator.unmount();
    },
  };
}
