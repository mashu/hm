// Chat: conversations with a station each, and stations to start one with.

import { api } from "../api.js";
import { h, fill } from "../dom.js";
import { bearer, day, hhmm, KEYS, pct, span } from "../format.js";
import { active, actions, badge, markRead, messagesChanged, partner, state } from "../messages.js";
import { store, watchAll } from "../store.js";

const ARCHIVED = "hm-archived-chats";

function archived() {
  try {
    return JSON.parse(localStorage.getItem(ARCHIVED) || "{}");
  } catch (_) {
    return {};
  }
}

function archive(call, seq) {
  const all = archived();
  all[call] = seq;
  try {
    localStorage.setItem(ARCHIVED, JSON.stringify(all));
  } catch (_) {
    // Archiving lasts as long as this page, then.
  }
}

export function mount(root, arg) {
  let current = arg ? decodeURIComponent(arg).toUpperCase() : null;
  let showArchived = false;
  const convos = h("ul", { class: "rail-list" });
  const onAir = h("ul", { class: "rail-list" });
  const trusted = h("ul", { class: "rail-list" });
  const moreButton = h("button", { type: "button", class: "btn quiet small", hidden: true,
    onclick: () => { showArchived = !showArchived; render(); } });
  const title = h("h2");
  const sub = h("p", { class: "muted small" });
  const archiveButton = h("button", { type: "button", class: "btn quiet small" }, "Archive");
  const clearButton = h("button", { type: "button", class: "btn danger small" }, "Clear history");
  const head = h("div", { class: "thread-head" }, title,
    h("div", { class: "row end" }, archiveButton, clearButton, h("a", { class: "btn quiet small", href: "#chat" }, "Close")), sub);
  const lines = h("ol", { class: "lines", "aria-live": "polite" });
  const input = h("textarea", { rows: 1, placeholder: "Message", "aria-label": "Message", required: true });
  const composer = h("form", { class: "composer", autocomplete: "off" }, input, h("button", { class: "btn", type: "submit" }, "Send"));
  const newCall = h("input", { maxlength: 9, placeholder: "Callsign", "aria-label": "Start a chat with", spellcheck: false, required: true });
  const shell = h("div", { class: "chat" },
    h("div", { class: "card rail" },
      h("form", { class: "row", autocomplete: "off", onsubmit: (e) => {
        e.preventDefault();
        location.hash = "#chat/" + encodeURIComponent(newCall.value.trim().toUpperCase());
        newCall.value = "";
      } }, newCall, h("button", { class: "btn", type: "submit" }, "Chat")),
      h("div", { class: "scroll" },
        h("h3", { class: "eyebrow" }, "Chats"), convos, moreButton,
        h("h3", { class: "eyebrow" }, "On frequency"), onAir,
        h("h3", { class: "eyebrow" }, "Trusted"), trusted)),
    h("div", { class: "card thread" }, head, lines, composer));
  root.replaceChildren(shell);

  const grow = () => {
    input.style.height = "auto";
    input.style.height = Math.min(input.scrollHeight, 160) + "px";
  };
  input.addEventListener("input", grow);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      composer.requestSubmit();
    }
  });
  composer.addEventListener("submit", async (e) => {
    e.preventDefault();
    if (!input.value.trim() || !current) return;
    try {
      await api.post("/api/send", { to: current, text: input.value });
      input.value = "";
      grow();
      messagesChanged();
    } catch (error) {
      alert("Not sent: " + error.message);
    }
  });
  archiveButton.addEventListener("click", () => {
    const mine = (store.get("chats") || []).filter((m) => partner(m) === current);
    archive(current, Math.max(0, ...mine.map((m) => m.seq)));
    location.hash = "#chat";
  });
  clearButton.addEventListener("click", async () => {
    if (!confirm("Delete the local history with " + current + "? Messages still on their way are kept.")) return;
    try {
      const result = await api.del("/api/conversations/" + encodeURIComponent(current));
      messagesChanged();
      if (!result.active) location.hash = "#chat";
    } catch (error) {
      alert("Could not clear the chat: " + error.message);
    }
  });

  let chats = [];
  let status = null;
  let trust = { stations: [] };

  function render() {
    renderConvos();
    renderStations();
    renderThread();
  }

  function renderConvos() {
    const byPeer = new Map();
    for (const m of chats) {
      const p = partner(m);
      if (!byPeer.has(p)) byPeer.set(p, { last: m, unread: 0 });
      if (m.state === "Unread") byPeer.get(p).unread++;
    }
    if (current && !byPeer.has(current)) byPeer.set(current, { last: null, unread: 0 });
    const hidden = archived();
    let folded = 0;
    const items = [];
    for (const [p, c] of byPeer) {
      const at = Number(hidden[p] || 0);
      if (p !== current && !showArchived && at && (!c.last || c.last.seq <= at)) {
        folded++;
        continue;
      }
      const last = c.last;
      const preview = last ? (last.direction === "out" ? state(last)[0] + " · You: " : "") + (last.text || "") : "No lines yet";
      items.push(h("li", null, h("a", { href: "#chat/" + encodeURIComponent(p), "aria-current": p === current ? "true" : null },
        h("span", { class: "name" }, p, c.unread ? h("span", { class: "count" }, c.unread) : null),
        h("span", { class: "meta" }, last ? hhmm(last.at) : ""),
        h("span", { class: "last" }, preview))));
    }
    fill(convos, items.length ? items : h("li", { class: "empty" }, folded ? "All chats are archived." : "No chats yet. Pick a station below or type a callsign."));
    moreButton.hidden = folded === 0 && !showArchived;
    moreButton.textContent = showArchived ? "Hide archived" : "Show archived (" + folded + ")";
  }

  function renderStations() {
    const heard = status.heard || [];
    fill(onAir, heard.length
      ? heard.slice(0, 12).map((st) => h("li", null, h("a", { href: "#chat/" + encodeURIComponent(st.station) },
        h("span", { class: "name" }, st.station), h("span", { class: "meta" }, span(st.ago) + " ago"),
        h("span", { class: "last" }, [st.locator, st.key ? KEYS[st.key]?.[0] : null].filter(Boolean).join(" · ") || "Heard on the air"))))
      : h("li", { class: "empty" }, status.radio === null ? "No radio configured." : "Nobody heard yet."));
    fill(trusted, trust.stations.length
      ? trust.stations.map((st) => h("li", null, h("a", { href: "#chat/" + encodeURIComponent(st.station) },
        h("span", { class: "name" }, st.station), h("span", { class: "meta" }, "trusted"),
        h("span", { class: "last" }, st.note || "Start a chat"))))
      : h("li", { class: "empty" }, "Nobody trusted yet (Settings › Trusted stations)."));
  }

  function renderThread() {
    shell.classList.toggle("open", !!current);
    head.hidden = composer.hidden = !current;
    if (!current) {
      fill(lines, h("li", { class: "placeholder" }, "Pick a conversation, someone on frequency or a trusted station, or type a callsign. Lines go by radio or internet, whichever reaches them."));
      return;
    }
    title.textContent = current;
    const mine = chats.filter((m) => partner(m) === current).reverse();
    const items = [];
    let lastDay = "";
    for (const m of mine) {
      if (day(m.at) !== lastDay) items.push(h("li", { class: "day" }, (lastDay = day(m.at))));
      const tone = m.state === "Failed" ? "failed" : m.state === "Cancelled" ? "cancelled" : m.direction === "in" && !m.verified ? "unverified" : null;
      items.push(h("li", { class: ["bubble", m.direction === "out" ? "mine" : null, tone], title: state(m)[1] },
        h("span", null, m.text ?? "(no text)"),
        h("span", { class: "footer" }, badge(m), h("span", { class: "meta" }, hhmm(m.at)), actions(m)),
        m.seq_gap ? h("span", { class: "meta gap" }, m.seq_gap) : null));
    }
    fill(lines, items.length ? items : h("li", { class: "placeholder" }, "No lines with " + current + " yet. Say hello."));
    lines.scrollTop = lines.scrollHeight;
    const heard = (status.heard || []).find((x) => x.station === current);
    const t = trust.stations.find((x) => x.station === current);
    const paths = (status.estimates || []).filter((e) => e.station === current)
      .map((e) => bearer(e.bearer).label + " " + pct(e.success) + " now");
    sub.textContent = [
      t ? "trusted" + (t.note ? " · " + t.note : "") : "not trusted",
      heard ? "heard " + span(heard.ago) + " ago" : null,
      heard?.locator ? heard.locator + (heard.distance_km != null ? ", " + heard.distance_km + " km" : "") : null,
      (status.internet_peers || []).includes(current) ? "linked over the internet" : null,
      ...paths,
    ].filter(Boolean).join(" · ");
    const busy = mine.some(active);
    archiveButton.disabled = !mine.length || busy;
    archiveButton.title = busy ? "Wait for or drop pending messages first" : "Hide this chat until a new line arrives";
    clearButton.disabled = !mine.length;
    if (!document.hidden) for (const m of mine) if (m.state === "Unread") markRead(m).catch(() => {});
  }

  const stop = watchAll(["chats", "status", "trust"], (c, st, tr) => {
    chats = c;
    status = st;
    trust = tr;
    render();
  });
  const seen = () => !document.hidden && current && render();
  document.addEventListener("visibilitychange", seen);
  if (current) input.focus();

  return {
    update(next) {
      current = next ? decodeURIComponent(next).toUpperCase() : null;
      if (status) render();
      if (current) input.focus();
    },
    unmount() {
      stop();
      document.removeEventListener("visibilitychange", seen);
    },
  };
}
