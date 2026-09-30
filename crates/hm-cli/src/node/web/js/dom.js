// Building the page without HTML strings: every text, most of it from the
// air, is set as text, never parsed. Styles go through the CSSOM, so the
// page runs under a policy that forbids inline style.

const SVG = "http://www.w3.org/2000/svg";
const PROPERTIES = new Set(["value", "checked", "disabled", "hidden", "selected", "required", "htmlFor", "type"]);

function append(node, children) {
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    node.append(child instanceof Node ? child : String(child));
  }
  return node;
}

function assign(node, props, svg) {
  for (const [key, value] of Object.entries(props || {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key.startsWith("on") && typeof value === "function") {
      node.addEventListener(key.slice(2).toLowerCase(), value);
    } else if (key === "class") {
      node.setAttribute("class", Array.isArray(value) ? value.filter(Boolean).join(" ") : value);
    } else if (key === "style") {
      Object.assign(node.style, value);
    } else if (key === "dataset") {
      Object.assign(node.dataset, value);
    } else if (!svg && PROPERTIES.has(key)) {
      node[key] = value;
    } else {
      node.setAttribute(key, value === true ? "" : String(value));
    }
  }
  return node;
}

/** An HTML element: `h("a", {href, class, onclick}, "text", child, [more])`. */
export function h(tag, props, ...children) {
  return append(assign(document.createElement(tag), props, false), children);
}

/** An SVG element, the same way. */
export function s(tag, props, ...children) {
  return append(assign(document.createElementNS(SVG, tag), props, true), children);
}

export const byId = (id) => document.getElementById(id);

/** Replace what `node` holds. */
export function fill(node, ...children) {
  node.replaceChildren();
  return append(node, children);
}

/** A button that runs `action`, disabled while it does; failures are shown. */
export function action(label, run, props = {}) {
  const button = h("button", { type: "button", ...props }, label);
  button.addEventListener("click", async () => {
    button.disabled = true;
    try {
      await run(button);
    } catch (error) {
      alert(error.message);
    } finally {
      button.disabled = false;
    }
  });
  return button;
}

/** A line of feedback under a form: `note(p, "Saved.", "ok")`. */
export function note(node, text, tone = "") {
  node.className = ["note", tone].filter(Boolean).join(" ");
  node.textContent = text;
}
