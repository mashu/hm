// The node's API: requests with the access token, and the event stream that
// says what changed.

const TOKEN = "hm-token";

// The token comes from the `#token=` link `hm node` prints, or the form; it
// stays in this browser.
export function takeTokenFromLink() {
  if (!location.hash.startsWith("#token=")) return;
  saveToken(decodeURIComponent(location.hash.slice(7)));
  history.replaceState(null, "", location.pathname + "#chat");
}

export function saveToken(token) {
  try {
    localStorage.setItem(TOKEN, token);
  } catch (_) {
    sessionToken = token;
  }
}

let sessionToken = "";
function token() {
  try {
    return localStorage.getItem(TOKEN) || sessionToken;
  } catch (_) {
    return sessionToken;
  }
}

/** Called when the node refuses the token; the page asks for another. */
export const onUnauthorized = new Set();

export class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function request(method, path, body) {
  const headers = { Authorization: "Bearer " + token() };
  if (body !== undefined) headers["Content-Type"] = "application/json";
  const response = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (response.status === 401) for (const f of onUnauthorized) f();
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw new ApiError(response.status, data.error || response.statusText);
  return data;
}

export const api = {
  get: (path) => request("GET", path),
  post: (path, body) => request("POST", path, body ?? {}),
  patch: (path, body) => request("PATCH", path, body),
  del: (path) => request("DELETE", path),
};

/**
 * Follow `/api/events`, calling `on(name)` for each change (`message`,
 * `status`, `settings`) and `live(state)` as the stream comes and goes;
 * reconnects with backoff, and calls `on("all")` on each (re)connection,
 * since changes may have been missed meanwhile.
 */
export async function follow(on, live) {
  let wait = 1000;
  for (;;) {
    try {
      const response = await fetch("/api/events", {
        headers: { Authorization: "Bearer " + token(), Accept: "text/event-stream" },
      });
      if (response.status === 401) for (const f of onUnauthorized) f();
      if (!response.ok || !response.body) throw new Error(response.statusText);
      live("live");
      on("all");
      wait = 1000;
      const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
      let buffer = "";
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += value;
        let end;
        while ((end = buffer.indexOf("\n\n")) >= 0) {
          for (const line of buffer.slice(0, end).split("\n")) {
            if (line.startsWith("data:")) on(line.slice(5).trim());
          }
          buffer = buffer.slice(end + 2);
        }
      }
    } catch (_) {
      // Reconnect below.
    }
    live("reconnecting");
    await new Promise((resolve) => setTimeout(resolve, wait));
    wait = Math.min(wait * 2, 30000);
  }
}
