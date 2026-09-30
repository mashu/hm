// What becomes of a message, in words; what can be done to one; the dialog
// with its decoded metadata and raw signed object.

import { api } from "./api.js";
import { byId, h } from "./dom.js";
import { hhmm, plural } from "./format.js";
import { store } from "./store.js";

export const partner = (m) => (m.direction === "in" ? m.from || m.peer : m.peer);
export const active = (m) => m.direction === "out" && (m.state === "Queued" || m.state === "InTransit");

/** [label, detail, tone] for a message's state. */
export function state(m) {
  if (m.kind === "Bulletin" && m.direction === "out") {
    if (m.state === "Delivered") return ["Published", "Sent (no per-listener receipts)", "ok"];
    if (m.state === "Failed") return ["Failed", "Not published: " + (m.note || "gave up"), "bad"];
    if (m.state === "Cancelled") return ["Cancelled", "Dropped from the local queue", ""];
    return ["Pending", "Waiting for a path that reaches listeners", "warn"];
  }
  if (m.direction === "in") {
    const sig = m.verified ? "signature verified" : "signature not verified";
    return m.state === "Unread"
      ? ["New", "Received · unread · " + sig, m.verified ? "ok" : "warn"]
      : ["Received", "Received · " + sig, m.verified ? "ok" : "warn"];
  }
  switch (m.state) {
    case "Delivered":
      return ["Delivered", "Delivered" + (m.delivered_by ? " by " + m.delivered_by : "") +
        (m.verified ? " · receipt verified" : " · destination receipt received"), "ok"];
    case "DeliveredUnconfirmed":
      return ["Delivered?", "Custody handed on · no receipt from the destination" + (m.note ? " · " + m.note : ""), "warn"];
    case "Failed":
      return ["Failed", "Not delivered: " + (m.note || "gave up"), "bad"];
    case "Cancelled":
      return ["Cancelled", "Dropped from the local queue", ""];
    case "InTransit":
      return ["In transit", "Next hop has custody · awaiting the destination's receipt", "warn"];
    default: {
      const next = m.next_attempt * 1000 > Date.now() ? " · next try " + hhmm(m.next_attempt) : "";
      const tries = m.attempts ? " · " + plural(m.attempts, "attempt") : "";
      return ["Pending", "Queued" + tries + next + (m.note ? " · " + m.note : ""), "warn"];
    }
  }
}

export function badge(m) {
  const [label, detail, tone] = state(m);
  return h("span", { class: ["pill", tone], title: detail }, label);
}

/** Load again everything a message change touches. */
export function messagesChanged() {
  store.changed("message");
}

const reading = new Set();

/** Mark `m` read (once, however often it is shown before the store catches up). */
export async function markRead(m) {
  if (reading.has(m.id)) return;
  reading.add(m.id);
  try {
    await api.post("/api/read/" + m.id);
  } catch (error) {
    reading.delete(m.id);
    throw error;
  }
  messagesChanged();
}

async function drop(m) {
  await api.del("/api/messages/" + m.id);
  messagesChanged();
}

async function remove(m) {
  if (!confirm("Delete this local copy for good? Copies already delivered elsewhere stay.")) return;
  await api.del("/api/messages/" + m.id);
  const dialog = byId("message-detail");
  if (dialog.open) dialog.close();
  messagesChanged();
}

const link = (label, run, props = {}) => {
  const b = h("button", { type: "button", class: ["link", props.danger ? "danger" : null], "aria-label": props.aria || label }, label);
  b.addEventListener("click", () => run().catch((e) => alert(e.message)));
  return b;
};

/** Details, and Drop (queued) or Delete (not in flight). */
export function actions(m) {
  return h("span", { class: "actions" },
    link("Details", () => openDetail(m), { aria: "View message details and raw signed object" }),
    m.direction === "out" && m.state === "Queued"
      ? link("Drop", () => drop(m), { danger: true, aria: "Drop this queued message" })
      : !active(m) ? link("Delete", () => remove(m), { danger: true, aria: "Delete this local message copy" }) : null,
    m.direction === "in" && m.state === "Unread" ? link("Mark read", () => markRead(m)) : null);
}

/** The dialog with everything about one message. */
export async function openDetail(m) {
  const dialog = byId("message-detail");
  const body = dialog.querySelector(".detail-body");
  body.replaceChildren(h("p", { class: "muted" }, "Loading…"));
  dialog.querySelector("h2").textContent = "Message " + m.id.slice(0, 12);
  if (!dialog.open) dialog.showModal();
  const detail = await api.get("/api/messages/" + m.id);
  const [, text, tone] = state(detail);
  const { raw_hex: raw, raw_bytes: bytes, ...decoded } = detail;
  const rawBlock = h("pre", { class: "raw" }, raw);
  body.replaceChildren(
    h("p", { class: ["note", tone] }, text),
    h("h3", null, "Decoded metadata"),
    h("pre", null, JSON.stringify(decoded, null, 2)),
    h("h3", null, "Raw signed object ", h("span", { class: "muted" }, "(" + bytes + " bytes)")),
    rawBlock,
    h("div", { class: "row" },
      link("Copy raw hex", async () => navigator.clipboard.writeText(raw)),
      !active(detail) ? link("Delete local copy", () => remove(detail), { danger: true }) : null));
}
