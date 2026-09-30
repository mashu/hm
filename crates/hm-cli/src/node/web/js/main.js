// The page: the header with the station and its bearers, the views behind
// `#name/argument` links, the access token, and the node's event stream.

import { follow, onUnauthorized, saveToken, takeTokenFromLink } from "./api.js";
import { byId } from "./dom.js";
import { bearer } from "./format.js";
import { store, watchAll } from "./store.js";
import * as beliefs from "./views/beliefs.js";
import * as bulletins from "./views/bulletins.js";
import * as chat from "./views/chat.js";
import * as mail from "./views/mail.js";
import * as network from "./views/network.js";
import * as settings from "./views/settings.js";

const VIEWS = { chat, mail, bulletins, network, beliefs, settings };

// ---- Views ----------------------------------------------------------------

let shown = null;

function route() {
  const [name, arg] = location.hash.slice(1).split("/");
  const view = VIEWS[name] ? name : "chat";
  for (const a of document.querySelectorAll("nav.tabs a")) {
    if (a.dataset.view === view) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  }
  if (shown?.name === view && shown.mounted.update) {
    shown.mounted.update(arg);
    return;
  }
  shown?.mounted.unmount();
  const root = byId("view");
  root.dataset.view = view;
  shown = { name: view, mounted: VIEWS[view].mount(root, arg) };
}

// ---- Header ----------------------------------------------------------------

function chip(id, text, tone, title) {
  const c = byId(id);
  c.hidden = text === null;
  if (text === null) return;
  c.className = ["chip", tone].filter(Boolean).join(" ");
  c.textContent = text;
  c.title = title || "";
}

function count(id, n) {
  const b = byId(id);
  b.hidden = !n;
  b.textContent = String(n);
}

store.watch("status", (s) => {
  byId("call").textContent = s.call;
  document.title = s.call + " · hm";
  chip("chip-grid", s.locator, "plain", "Our grid square, sent in beacons");
  chip("chip-radio", s.radio === null ? "No packet radio" : s.radio ? "Packet radio up" : "Packet radio down",
    s.radio === null ? "" : s.radio ? "ok" : "bad", (s.radio_via ? s.radio_via + ": " : "") + bearer("radio").tip);
  chip("chip-net", !s.internet_listen && !s.internet_peers.length ? null
    : s.internet_peers.length ? "Internet: " + s.internet_peers.join(", ") : "Internet: no links",
  s.internet_peers.length ? "ok" : "warn", bearer("internet").tip);
  chip("chip-modem", s.modem === null ? null : !s.modem ? "ARQ down" : s.modem_peer ? "ARQ: " + s.modem_peer : "ARQ ready",
    s.modem ? "ok" : "bad", (s.modem_via ? s.modem_via + ": " : "") + bearer("modem").tip);
  byId("token-form").hidden = true;
});
store.watch("chats", (list) => count("badge-chat", list.filter((m) => m.state === "Unread").length));
store.watch("inbox", (list) => count("badge-mail", list.filter((m) => m.state === "Unread").length));
store.watch("bulletins", (list) => count("badge-bulletins", list.filter((m) => m.direction === "in" && m.state === "Unread").length));
watchAll(["outbox"], (list) => count("badge-beliefs", list.filter((m) => m.state === "Queued" || m.state === "InTransit").length));

// ---- Token -------------------------------------------------------------------

onUnauthorized.add(() => (byId("token-form").hidden = false));
byId("token-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const input = byId("token");
  saveToken(input.value.trim());
  input.value = "";
  store.changed("all");
});

// ---- Start -------------------------------------------------------------------

takeTokenFromLink();
window.addEventListener("hashchange", route);
route();
follow((change) => store.changed(change), (state) =>
  chip("chip-live", state === "live" ? "Live" : "Reconnecting…", state === "live" ? "ok" : "warn",
    state === "live" ? "Changes arrive as they happen" : "Lost the node's event stream; trying again"));

// Stations age out of the heard list and paths drift without an event: look again now and then.
setInterval(() => !document.hidden && store.refresh("status"), 30000);
