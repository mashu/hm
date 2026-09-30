// Mail: a form to write one, and the inbox and sent mail.

import { api } from "../api.js";
import { fill, h, note } from "../dom.js";
import { day, hhmm } from "../format.js";
import { actions, badge, messagesChanged, state } from "../messages.js";
import { store } from "../store.js";

export function mount(root) {
  let box = "inbox";
  const status = h("p", { class: "note", role: "status" });
  const field = (id, label, input) => [h("label", { htmlFor: id }, label), Object.assign(input, { id })];
  const to = h("input", { name: "to", required: true, maxlength: 9, placeholder: "SO5KM-1", spellcheck: false, class: "call" });
  const subject = h("input", { name: "subject", maxlength: 128, required: true });
  const text = h("textarea", { name: "text", rows: 8, required: true });
  const precedence = h("select", { name: "precedence" },
    ["routine", "priority", "immediate", "flash"].map((p) => h("option", { value: p }, p[0].toUpperCase() + p.slice(1))));
  const form = h("form", { class: "card", autocomplete: "off" },
    h("h2", null, "New message"),
    field("to", "To", to), field("subject", "Subject", subject), field("text", "Message", text),
    field("precedence", "Precedence", precedence),
    h("button", { class: "btn", type: "submit" }, "Queue message"), status);
  const tabs = ["inbox", "sent"].map((name) =>
    h("button", { type: "button", role: "tab", "aria-selected": String(name === box), onclick: () => show(name) },
      name === "inbox" ? "Inbox" : "Sent"));
  const list = h("ol", { class: "log", "aria-live": "polite" });
  root.replaceChildren(h("div", { class: "split" }, form,
    h("div", { class: "card" }, h("div", { class: "subtabs", role: "tablist" }, tabs), list)));

  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const body = { to: to.value.trim().toUpperCase(), text: text.value, subject: subject.value.trim(), precedence: precedence.value };
    note(status, "Queuing…");
    try {
      await api.post("/api/send", body);
      note(status, "Queued for " + body.to + ". It goes out when a path allows; follow it under Sent, or under Beliefs for why.", "ok");
      text.value = subject.value = "";
      messagesChanged();
    } catch (error) {
      note(status, "Not queued: " + error.message, "bad");
    }
  });

  let stop = () => {};
  function show(name) {
    box = name;
    tabs.forEach((t, i) => t.setAttribute("aria-selected", String(["inbox", "sent"][i] === name)));
    stop();
    stop = store.watch(name, render);
  }

  function render(messages) {
    fill(list, messages.length ? messages.map(row) : h("li", { class: "empty" }, box === "inbox"
      ? "No mail yet."
      : "No mail sent yet. Write one with the form."));
  }

  function row(m) {
    const incoming = m.direction === "in";
    const who = incoming ? m.from || m.peer : m.peer;
    const [, detail, tone] = state(m);
    return h("li", { class: ["p" + Math.min(m.precedence, 3), m.state === "Unread" ? "unread" : null] },
      h("span", { class: "when" }, day(m.at).slice(5), h("br"), hhmm(m.at)),
      h("span", { class: "who" }, who),
      h("span", { class: "body" },
        m.subject ? h("strong", { class: "subject" }, m.subject) : null,
        h("span", null, m.text ?? "(no text)"),
        h("span", { class: "footer" }, badge(m),
          h("span", { class: ["small", tone] }, incoming
            ? (m.verified ? "Signature verified" : "Signature not verified (sender not trusted here)") +
              (m.from && m.from !== m.peer ? ", via " + m.peer : "")
            : detail),
          actions(m))));
  }

  show(box);
  return { unmount: () => stop() };
}
