// Forms described as data: each field names where its value lives in the
// settings (a dotted path), how it is shown and what it may be. `Form`
// builds the inputs, fills them from a settings object, checks them and
// collects them back into one.

import { h } from "./dom.js";

export const getPath = (obj, path) => path.split(".").reduce((o, k) => (o == null ? undefined : o[k]), obj);

export function setPath(obj, path, value) {
  const keys = path.split(".");
  const last = keys.pop();
  let o = obj;
  for (const k of keys) o = o[k] ??= {};
  o[last] = value;
}

let ids = 0;

/** One field's control: its element, and how to read, write and check it. */
function control(f) {
  const id = "f" + ++ids;
  const hint = f.hint ? h("span", { class: "muted" }, " (" + f.hint + ")") : null;
  if (f.type === "check") {
    const input = h("input", { id, type: "checkbox" });
    return {
      node: h("label", { class: "check", htmlFor: id }, input, h("span", null, f.label, f.hint ? h("span", { class: "muted small" }, f.hint) : null)),
      inputs: [input],
      get: () => input.checked,
      set: (v) => (input.checked = !!v),
    };
  }
  if (f.type === "select" || f.type === "choice") {
    const select = h("select", { id }, f.options.map(([value, label]) => h("option", { value }, label)));
    if (f.type === "select") {
      return {
        node: [h("label", { htmlFor: id }, f.label, hint), select],
        inputs: [select],
        get: () => select.value,
        set: (v) => (select.value = String(v ?? f.options[0][0])),
      };
    }
    // A choice among common values, or one of your own.
    select.append(h("option", { value: "custom" }, "Other…"));
    const own = h("input", { type: f.numeric ? "number" : "text", placeholder: f.placeholder || "", spellcheck: false, hidden: true, "aria-label": f.label + ": your own" });
    select.addEventListener("change", () => {
      own.hidden = select.value !== "custom";
      if (!own.hidden) own.focus();
    });
    return {
      node: [h("label", { htmlFor: id }, f.label, hint), select, own],
      inputs: [select, own],
      get: () => {
        const v = select.value === "custom" ? own.value.trim() : select.value;
        return f.numeric ? Number(v) : v;
      },
      set: (v) => {
        const text = String(v ?? f.options[0][0]);
        const known = f.options.some(([value]) => value === text);
        select.value = known ? text : "custom";
        own.hidden = known;
        own.value = known ? "" : text;
      },
      check: () => (select.value === "custom" && !own.value.trim() ? [own, f.label + " is needed"] : null),
    };
  }
  const number = f.type === "number";
  const input = h("input", {
    id, type: number ? "number" : "text", spellcheck: false, placeholder: f.placeholder || "",
    min: f.min, max: f.max, step: f.step ?? (number ? 1 : null), required: !!f.required,
  });
  return {
    node: [h("label", { htmlFor: id }, f.label, hint), input],
    inputs: [input],
    get: () => (number ? Number(input.value) : input.value.trim()),
    set: (v) => (input.value = v ?? ""),
  };
}

/**
 * Fields laid out in groups: `[{title?, when?, advanced?, columns?, fields: [...]}]`.
 * A group `when` a checkbox field (by path) is off is hidden and not checked.
 */
export class Form {
  constructor(groups) {
    this.fields = [];
    this.nodes = groups.map((group) => {
      const controls = group.fields.map((f) => {
        const c = { ...control(f), f };
        this.fields.push(c);
        return h("div", { class: "field" }, c.node);
      });
      const body = h("div", { class: ["fields", group.columns ? "cols-" + group.columns : null] }, controls);
      const node = group.advanced
        ? h("details", { class: "advanced" }, h("summary", null, group.title, group.hint ? h("span", { class: "muted" }, " " + group.hint) : null), body)
        : h("div", { class: "group" }, group.title ? h("h3", { class: "eyebrow" }, group.title) : null,
          group.lede ? h("p", { class: "muted small" }, group.lede) : null, body);
      return { node, group };
    });
    for (const { node, group } of this.nodes) {
      if (!group.when) continue;
      const toggle = this.fields.find((c) => c.f.path === group.when);
      const sync = () => {
        node.hidden = !toggle.get();
        for (const c of this.fields) if (node.contains(c.inputs[0])) c.inputs.forEach((i) => (i.disabled = node.hidden));
      };
      toggle.inputs[0].addEventListener("change", sync);
      this.syncs = [...(this.syncs || []), sync];
    }
  }

  /** The input of the field at `path`. */
  input(path) {
    return this.fields.find((c) => c.f.path === path).inputs[0];
  }

  get elements() {
    return this.nodes.map((n) => n.node);
  }

  fill(settings) {
    for (const c of this.fields) c.set(getPath(settings, c.f.path));
    for (const sync of this.syncs || []) sync();
  }

  /** The first problem, as [input, message], or null. */
  problem() {
    for (const c of this.fields) {
      if (c.inputs[0].disabled) continue;
      for (const input of c.inputs) if (!input.hidden && !input.checkValidity()) return [input, input.validationMessage];
      const bad = c.check?.();
      if (bad) return bad;
    }
    return null;
  }

  /** Write every field into `body`. */
  collect(body) {
    for (const c of this.fields) if (!c.inputs[0].disabled) setPath(body, c.f.path, c.get());
    return body;
  }
}
