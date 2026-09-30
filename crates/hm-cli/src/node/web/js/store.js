// What the page knows, in named slices, each loaded from the API and watched
// by the views that show it. Views render from the store and never fetch:
// one request per slice however many views show it, a slice nobody shows is
// not fetched, and a burst of changes while a request is out costs one more
// request, not one each.

import { api } from "./api.js";

const LOADERS = {
  status: () => api.get("/api/status"),
  insight: () => api.get("/api/insight"),
  trust: () => api.get("/api/trust"),
  settings: () => api.get("/api/settings"),
  chats: () => api.get("/api/messages?direction=all&kind=chat&limit=1000"),
  inbox: () => api.get("/api/messages?direction=in&kind=mail&limit=500"),
  sent: () => api.get("/api/messages?direction=out&kind=mail&limit=200"),
  bulletins: () => api.get("/api/messages?direction=all&kind=bulletin&limit=200"),
  outbox: () => api.get("/api/messages?direction=out&limit=200"),
};

/** The slices each change the node announces may have changed. */
const CHANGES = {
  message: ["chats", "inbox", "sent", "bulletins", "outbox", "insight"],
  status: ["status", "insight"],
  settings: ["settings", "trust", "status"],
};
CHANGES.all = Object.keys(LOADERS);

class Store {
  #values = new Map();
  #watchers = new Map();
  #loading = new Map();
  #again = new Set();

  get(name) {
    return this.#values.get(name);
  }

  /** Call `f(value)` with the slice now (if loaded) and on every change; returns the unwatch. */
  watch(name, f) {
    if (!this.#watchers.has(name)) this.#watchers.set(name, new Set());
    this.#watchers.get(name).add(f);
    if (this.#values.has(name)) f(this.#values.get(name));
    else this.refresh(name);
    return () => this.#watchers.get(name).delete(f);
  }

  watched(name) {
    return (this.#watchers.get(name)?.size ?? 0) > 0;
  }

  /** Load `name` again; while a load is out, one more follows it. */
  refresh(name) {
    if (this.#loading.has(name)) {
      this.#again.add(name);
      return this.#loading.get(name);
    }
    const load = LOADERS[name]()
      .then((value) => this.set(name, value))
      .catch(() => {})
      .finally(() => {
        this.#loading.delete(name);
        if (this.#again.delete(name)) this.refresh(name);
      });
    this.#loading.set(name, load);
    return load;
  }

  set(name, value) {
    this.#values.set(name, value);
    for (const f of this.#watchers.get(name) ?? []) f(value);
  }

  /** The node says `change` happened: load again what it touches and is shown. */
  changed(change) {
    for (const name of CHANGES[change] ?? []) {
      if (this.watched(name)) this.refresh(name);
    }
  }
}

export const store = new Store();

/**
 * Watch several slices; `f(...values)` runs once all are loaded and again
 * after changes, once for all the changes of one turn of the event loop.
 */
export function watchAll(names, f) {
  let queued = false;
  let stopped = false;
  const run = () => {
    if (queued) return;
    queued = true;
    queueMicrotask(() => {
      queued = false;
      if (stopped) return;
      const values = names.map((name) => store.get(name));
      if (values.every((v) => v !== undefined)) f(...values);
    });
  };
  const stops = names.map((name) => store.watch(name, run));
  return () => {
    stopped = true;
    stops.forEach((stop) => stop());
  };
}

/**
 * Load `name` again every `secs` while it is watched and the page is in
 * view: for what changes without the node announcing it (beliefs drift
 * with time). Returns the stop.
 */
export function poll(name, secs) {
  const timer = setInterval(() => {
    if (!document.hidden && store.watched(name)) store.refresh(name);
  }, secs * 1000);
  return () => clearInterval(timer);
}
