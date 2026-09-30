// Bulletins: publishing to a group, and what was heard, by group.

import { api } from "../api.js";
import { fill, h, note } from "../dom.js";
import { hhmm } from "../format.js";
import { actions, badge, markRead, messagesChanged } from "../messages.js";
import { store } from "../store.js";

const groupOf = (m) => (m.to || []).find((t) => t.startsWith("group:"))?.slice(6) || "";

export function mount(root) {
  let filter = "";
  let bulletins = [];
  const status = h("p", { class: "note", role: "status" });
  const group = h("input", { id: "b-group", name: "group", required: true, maxlength: 32, placeholder: "SK-EMCOMM", spellcheck: false });
  const subject = h("input", { id: "b-subject", maxlength: 128 });
  const text = h("textarea", { id: "b-text", rows: 6, required: true });
  const form = h("form", { class: "card", autocomplete: "off" },
    h("h2", null, "Publish"),
    h("p", { class: "muted small" }, "Queued like mail: by packet radio to everyone in reach, and to linked internet peers. At most four an hour; no receipts."),
    h("label", { htmlFor: "b-group" }, "Group"), group,
    h("label", { htmlFor: "b-subject" }, "Subject ", h("span", { class: "muted" }, "(optional)")), subject,
    h("label", { htmlFor: "b-text" }, "Message"), text,
    h("button", { class: "btn", type: "submit" }, "Publish"), status);
  const filters = h("div", { class: "filters", "aria-label": "Filter by group" });
  const list = h("div", { "aria-live": "polite" });
  root.replaceChildren(h("div", { class: "split" }, form, h("div", { class: "card" }, h("h2", null, "Bulletins"), filters, list)));

  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    note(status, "Publishing…");
    try {
      const name = group.value.trim();
      await api.post("/api/send", { kind: "bulletin", group: name, to: name, text: text.value, subject: subject.value.trim() || undefined });
      note(status, "Queued. It goes out when a path allows.", "ok");
      text.value = subject.value = "";
      messagesChanged();
    } catch (error) {
      note(status, "Not published: " + error.message, "bad");
    }
  });

  function render() {
    const groups = [...new Set(bulletins.map(groupOf).filter(Boolean))].sort();
    const pick = (g) => h("button", { type: "button", "aria-pressed": String(filter === g), onclick: () => { filter = g; render(); } }, g || "All");
    fill(filters, pick(""), groups.map(pick));
    const shown = bulletins.filter((m) => !filter || groupOf(m) === filter);
    fill(list, shown.length ? shown.map((m) => {
      if (!document.hidden && m.state === "Unread") markRead(m).catch(() => {});
      return h("article", { class: "bulletin" },
        h("div", null, h("strong", null, groupOf(m) || "(no group)"),
          h("span", { class: "muted small" }, " · " + (m.direction === "out" ? "you" : m.from || m.peer) + " · " + hhmm(m.at) + " "), badge(m)),
        m.subject ? h("strong", { class: "subject" }, m.subject) : null,
        h("div", { class: "body" }, m.text ?? "(no text)"),
        h("span", { class: "footer" }, actions(m)));
    }) : h("p", { class: "empty" }, filter ? "No bulletins in " + filter + " yet." : "No bulletins yet. Publish one, or wait for one to arrive."));
  }

  const stop = store.watch("bulletins", (b) => {
    bulletins = b;
    render();
  });
  return { unmount: stop };
}
