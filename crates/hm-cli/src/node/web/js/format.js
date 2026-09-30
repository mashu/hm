// Numbers, times and chances as people read them. Times are UTC, as on the air.

export const nowSecs = () => Math.floor(Date.now() / 1000);

const iso = (unix) => new Date(unix * 1000).toISOString();
export const hhmm = (unix) => iso(unix).slice(11, 16) + "Z";
export const day = (unix) => iso(unix).slice(0, 10);

/** "40 s", "12 min", "3 h", "2 d". */
export function span(secs) {
  const s = Math.max(0, Math.round(secs));
  if (s < 90) return s + " s";
  if (s < 90 * 60) return Math.round(s / 60) + " min";
  if (s < 48 * 3600) return Math.round(s / 3600) + " h";
  return Math.round(s / 86400) + " d";
}

/** "12 min ago" or "in 3 h", from `now`. */
export function relative(unix, now = nowSecs()) {
  const d = unix - now;
  if (Math.abs(d) < 30) return "now";
  return d > 0 ? "in " + span(d) : span(-d) + " ago";
}

/** A chance as a percentage, never 0 % or 100 % unless it is. */
export function pct(p) {
  if (p === null || p === undefined || Number.isNaN(p)) return "–";
  if (p <= 0) return "0%";
  if (p >= 1) return "100%";
  if (p < 0.01) return "<1%";
  if (p > 0.99) return ">99%";
  return Math.round(p * 100) + "%";
}

/** "34% (21–48%)": a mean and its credible interval. */
export const interval = (i) => pct(i.mean) + " (" + pct(i.low) + "–" + pct(i.high) + ")";

export const km = (d) => (d < 10 ? d.toFixed(1) : Math.round(d).toLocaleString("en")) + " km";

export function compass(bearing) {
  return ["N", "NE", "E", "SE", "S", "SW", "W", "NW"][Math.round(bearing / 45) % 8] + " " + Math.round(bearing) + "°";
}

/** A number of faded observations: "3.4" below ten, whole above. */
export const count = (n) => (n < 10 ? n.toFixed(1) : Math.round(n).toString());

export const plural = (n, one, many = one + "s") => n + " " + (n === 1 ? one : many);

/** What bearers are called on screen. */
export const BEARERS = {
  radio: { label: "Packet radio", short: "radio", tip: "KISS TNC or the built-in AFSK modem; hm's own transfer engine" },
  internet: { label: "Internet", short: "internet", tip: "Authenticated QUIC link to another station" },
  modem: { label: "ARQ modem", short: "ARQ", tip: "VARA, ARDOP or Mercury: an external ARQ program" },
};
export const bearer = (id) => BEARERS[id] || { label: id, short: id, tip: "" };

/** How a beacon's key compares with the trusted one: text and tone. */
export const KEYS = {
  trusted: ["Key matches the one you trust", "ok"],
  unknown: ["Not among your trusted stations", "warn"],
  mismatch: ["Key differs from the one you trust: an impostor, or a new key", "bad"],
};
