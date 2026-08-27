const OWNED_CONTROL_SELECTOR = [
  ".sdsync-input-control",
  ".sdsync-select-control",
  ".sdsync-checkbox-control"
].join(", ");

const RESPONSIVE_FORM_SELECTOR = ".sdsync-settings-panel, .sdsync-horizontal-form";
const FORM_ROW_SELECTOR = ".sdsync-form-item";

function semanticTargets(owner) {
  if (owner.classList.contains("sdsync-checkbox-control")) {
    return owner.querySelectorAll('input[type="checkbox"]');
  }
  if (owner.classList.contains("sdsync-select-control")) {
    return owner.querySelectorAll('[role="combobox"], [aria-haspopup="listbox"]');
  }
  return owner.querySelectorAll('input:not([type="checkbox"]):not([type="radio"]), textarea');
}

function shellClass(owner) {
  if (owner.classList.contains("sdsync-checkbox-control")) return "sdsync-checkbox-shell";
  if (owner.classList.contains("sdsync-select-control")) return "sdsync-select-shell";
  return "sdsync-input-shell";
}

function markControlShells(root) {
  const owners = root.querySelectorAll(OWNED_CONTROL_SELECTOR);
  for (const owner of owners) {
    const typeClass = shellClass(owner);
    const targets = semanticTargets(owner);
    for (const target of targets) {
      let shell = target.parentElement;
      while (shell && shell !== owner) {
        shell.classList.add("sdsync-control-shell", typeClass);
        shell = shell.parentElement;
      }
    }
  }
}

function markCheckboxParts(root) {
  const oldParts = root.querySelectorAll(
    ".sdsync-checkbox-label, .sdsync-checkbox-input, .sdsync-checkbox-glyph"
  );
  for (const oldPart of oldParts) {
    oldPart.classList.remove("sdsync-checkbox-label", "sdsync-checkbox-input", "sdsync-checkbox-glyph");
  }

  const owners = root.querySelectorAll(".sdsync-checkbox-control");
  for (const owner of owners) {
    const label = owner.matches("label") ? owner : owner.querySelector("label");
    if (label) label.classList.add("sdsync-checkbox-label");
    const input = owner.querySelector('input[type="checkbox"]');
    if (!input) continue;
    input.classList.add("sdsync-checkbox-input");
    const glyph = input.previousElementSibling;
    if (glyph && glyph !== label) glyph.classList.add("sdsync-checkbox-glyph");
  }
}

function markSelectParts(root) {
  const oldParts = root.querySelectorAll(
    ".sdsync-select-row, .sdsync-select-prefix, .sdsync-select-affordance"
  );
  for (const oldPart of oldParts) {
    oldPart.classList.remove("sdsync-select-row", "sdsync-select-prefix", "sdsync-select-affordance");
  }

  const owners = root.querySelectorAll(".sdsync-select-control");
  for (const owner of owners) {
    const targets = semanticTargets(owner);
    if (!targets.length || targets[0] === owner) continue;
    const target = targets[0];
    let row = target.parentElement;
    while (row && row.parentElement !== owner) row = row.parentElement;
    if (!row) continue;
    row.classList.add("sdsync-select-row");
    let controlChild = target;
    while (controlChild.parentElement && controlChild.parentElement !== row) {
      controlChild = controlChild.parentElement;
    }
    const prefix = controlChild.previousElementSibling;
    const affordance = controlChild.nextElementSibling;
    if (prefix) prefix.classList.add("sdsync-select-prefix");
    if (affordance) affordance.classList.add("sdsync-select-affordance");
  }
}

function markFormControlPaths(root) {
  const oldShells = root.querySelectorAll(".sdsync-form-control-shell, .sdsync-form-control-cell");
  for (const oldShell of oldShells) {
    oldShell.classList.remove("sdsync-form-control-shell", "sdsync-form-control-cell");
  }

  const rows = root.querySelectorAll(FORM_ROW_SELECTOR);
  for (const row of rows) {
    const control = row.querySelector(OWNED_CONTROL_SELECTOR + ", .sdsync-native-input");
    if (!control) continue;
    let cell = control;
    let shell = control.parentElement;
    while (shell && shell !== row) {
      shell.classList.add("sdsync-form-control-shell");
      cell = shell;
      shell = shell.parentElement;
    }
    if (cell.parentElement === row) cell.classList.add("sdsync-form-control-cell");
  }
}

function setCompactState(form) {
  form.classList.toggle("sdsync-compact-form", form.getBoundingClientRect().width <= 720);
}

/**
 * Adds package-owned hooks to DSM-rendered wrapper elements. The SDK does not
 * expose stable private wrapper classes, so CSS never needs to guess them or
 * rely on selectors newer than the declared Chrome 88 baseline.
 */
export function installControlLayout(root) {
  if (!root || typeof root.querySelectorAll !== "function") return () => {};

  let active = true;
  const observedForms = new Set();
  const resizeObserver = typeof ResizeObserver === "function"
    ? new ResizeObserver((entries) => {
      if (!active) return;
      for (const entry of entries) setCompactState(entry.target);
    })
    : null;

  const refresh = () => {
    if (!active) return;
    markControlShells(root);
    markCheckboxParts(root);
    markSelectParts(root);
    markFormControlPaths(root);
    const forms = new Set(root.querySelectorAll(RESPONSIVE_FORM_SELECTOR));
    if (resizeObserver) {
      for (const form of observedForms) {
        if (forms.has(form)) continue;
        resizeObserver.unobserve(form);
        observedForms.delete(form);
        form.classList.remove("sdsync-compact-form");
      }
    }
    for (const form of forms) {
      setCompactState(form);
      if (resizeObserver && !observedForms.has(form)) {
        observedForms.add(form);
        resizeObserver.observe(form);
      }
    }
  };

  let refreshQueued = false;
  const queueRefresh = () => {
    if (!active || refreshQueued) return;
    refreshQueued = true;
    Promise.resolve().then(() => {
      refreshQueued = false;
      if (!active) return;
      refresh();
    });
  };

  const mutationObserver = typeof MutationObserver === "function"
    ? new MutationObserver(queueRefresh)
    : null;
  if (mutationObserver) mutationObserver.observe(root, { childList: true, subtree: true });

  const onWindowResize = () => {
    if (active) refresh();
  };
  if (!resizeObserver && typeof window !== "undefined") window.addEventListener("resize", onWindowResize);
  refresh();

  return () => {
    if (!active) return;
    active = false;
    if (mutationObserver) mutationObserver.disconnect();
    if (resizeObserver) resizeObserver.disconnect();
    if (!resizeObserver && typeof window !== "undefined") window.removeEventListener("resize", onWindowResize);
    for (const form of observedForms) form.classList.remove("sdsync-compact-form");
    observedForms.clear();
  };
}
