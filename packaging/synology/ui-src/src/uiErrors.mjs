/**
 * The last line of defence against a silent, frozen dashboard.
 *
 * An exception that escapes a render, a watcher or an event handler used to land
 * in the browser console and nowhere else. When it escaped from inside Vue's
 * scheduler flush it also left the scheduler marked as "waiting", so no
 * component repainted again: every status badge kept its last text, green or
 * not, and nothing on screen said so.
 *
 * reportUiError() writes the error to the console and puts a banner in the
 * AppWindow's root element. The banner is plain DOM, built with textContent,
 * because the one situation it exists for is Vue no longer updating the page.
 * There is only ever one banner; a repeat updates its count.
 *
 * Imports only textSafety.mjs, which imports nothing.
 */

import { plainText } from "./textSafety.mjs";

const BANNER_CLASS = "sdsync-banner sdsync-ui-error";
const MESSAGE_LIMIT = 300;
const state = new WeakMap();

function describe(error) {
  return plainText(error && error.message ? error.message : String(error), MESSAGE_LIMIT);
}

function build(documentRef, reload) {
  const banner = documentRef.createElement("div");
  banner.setAttribute("class", BANNER_CLASS);
  banner.setAttribute("role", "alert");
  const row = documentRef.createElement("div");
  const title = documentRef.createElement("strong");
  title.textContent = "The dashboard hit an internal error";
  const detail = documentRef.createElement("span");
  const button = documentRef.createElement("button");
  button.setAttribute("type", "button");
  button.setAttribute("class", "sdsync-ui-error-reload");
  button.textContent = "Reload this window";
  button.addEventListener("click", reload);
  row.appendChild(title);
  row.appendChild(detail);
  row.appendChild(button);
  banner.appendChild(row);
  return { banner, detail };
}

function isElement(node) {
  return Boolean(node) && node.nodeType === 1 && typeof node.insertBefore === "function";
}

/**
 * Where the banner goes. The AppWindow's content is drawn by DSM's window
 * component, so the wrapper's own element is not necessarily where the page is;
 * the workspace column of the dashboard is, when it exists. The desktop card is
 * its own root and is its own host.
 */
function hostFor(root) {
  const own = root && root.$el;
  const classes = isElement(own) ? String(own.getAttribute("class") || "") : "";
  if (classes.split(" ").includes("sdsync-widget")) return own;
  if (typeof document !== "undefined" && typeof document.querySelector === "function") {
    const workspace = document.querySelector(".sdsync-workspace");
    if (isElement(workspace)) return workspace;
  }
  return isElement(own) ? own : null;
}

/**
 * `root` is the Vue instance whose element hosts the banner; `info` is Vue's
 * label for where the error was raised ("render", "v-on handler", ...).
 */
export function reportUiError(error, info, root) {
  const message = describe(error);
  if (typeof console !== "undefined" && console.error) console.error(`[sdsync] ${info || "error"}: ${message}`, error);
  const host = hostFor(root);
  const documentRef = (host && host.ownerDocument) || (typeof document !== "undefined" ? document : null);
  if (!host || !documentRef || typeof host.insertBefore !== "function") {
    // Nowhere to draw it. The console line above is all that can be said; the
    // error is still counted so a later report can say how many came before.
    return;
  }
  let entry = state.get(host);
  if (!entry || entry.banner.parentNode !== host) {
    const reload = () => {
      if (typeof location !== "undefined" && typeof location.reload === "function") location.reload();
    };
    entry = { ...build(documentRef, reload), count: 0 };
    state.set(host, entry);
    host.insertBefore(entry.banner, host.firstChild || null);
  }
  entry.count += 1;
  entry.detail.textContent = ` Parts of this page may be showing old information (${info || "error"}: ${message}). ${
    entry.count > 1 ? `(${entry.count} errors so far) ` : ""
  }Reload to recover.`;
}

/**
 * Vue 2 does not route a patch exception to errorCaptured. A render watcher
 * rethrows it, the scheduler's flush aborts, and the error reaches only the
 * global handler -- which is DSM's, not ours to replace -- while the scheduler
 * stays marked as flushing and never repaints again. A component that mixes this
 * in wraps its own patch so that failure becomes the banner above. The error is
 * rethrown: the DOM is half patched and nothing here pretends otherwise.
 */
export const patchGuard = {
  beforeCreate() {
    const original = this.__patch__;
    if (typeof original !== "function") return;
    this.__patch__ = (...args) => {
      try {
        return original.apply(this, args);
      } catch (error) {
        reportUiError(error, "patch", this.$root);
        throw error;
      }
    };
  }
};
