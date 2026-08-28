const OWNED_CONTROL_SELECTOR = [
  ".sdsync-input-control",
  ".sdsync-select-control",
  ".sdsync-checkbox-control"
].join(", ");

const RESPONSIVE_FORM_SELECTOR = ".sdsync-settings-panel, .sdsync-horizontal-form";
const FORM_ROW_SELECTOR = ".sdsync-form-item";
const APP_SHELL_SELECTOR = ".sdsync-app";
const OWNED_OVERLAY_SELECTOR = ".sdsync-select-dropdown";
const FIELD_TIP_SELECTOR = ".sdsync-field-tip";
const SHELL_MEDIUM_WIDTH = 980;
const SHELL_COMPACT_WIDTH = 720;
const FORM_COMPACT_WIDTH = 420;
const BOUNDARY_INSET = 8;
const OVERLAY_STYLE_PROPERTIES = [
  "position", "left", "right", "top", "bottom", "width", "max-width", "max-height"
];

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
  form.classList.toggle(
    "sdsync-compact-form",
    form.getBoundingClientRect().width <= FORM_COMPACT_WIDTH
  );
}

function matchingElements(root, selector) {
  const values = [];
  if (root && typeof root.matches === "function" && root.matches(selector)) values.push(root);
  if (root && typeof root.querySelectorAll === "function") {
    for (const element of root.querySelectorAll(selector)) values.push(element);
  }
  return values;
}

function setShellState(shell) {
  const width = shell.getBoundingClientRect().width;
  shell.classList.toggle("sdsync-medium-shell", width <= SHELL_MEDIUM_WIDTH);
  shell.classList.toggle("sdsync-compact-shell", width <= SHELL_COMPACT_WIDTH);
}

function clearShellState(shell) {
  shell.classList.remove("sdsync-medium-shell", "sdsync-compact-shell");
}

function clamp(value, minimum, maximum) {
  if (maximum < minimum) return minimum;
  return Math.min(Math.max(value, minimum), maximum);
}

function visibleShells(shells) {
  return Array.from(shells).filter((shell) => {
    const rect = shell.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  });
}

function distanceToRect(rect, pointX, pointY) {
  const x = clamp(pointX, rect.left, rect.right);
  const y = clamp(pointY, rect.top, rect.bottom);
  return ((pointX - x) ** 2) + ((pointY - y) ** 2);
}

function nearestShell(element, shells) {
  const candidates = visibleShells(shells);
  if (!candidates.length) return null;
  if (candidates.length === 1) return candidates[0];
  const rect = element.getBoundingClientRect();
  const centerX = rect.left + (rect.width / 2);
  const centerY = rect.top + (rect.height / 2);
  return candidates.reduce((nearest, shell) => {
    const shellRect = shell.getBoundingClientRect();
    const distance = distanceToRect(shellRect, centerX, centerY);
    return !nearest || distance < nearest.distance ? { shell, distance } : nearest;
  }, null).shell;
}

function rememberInlineStyles(element, originals) {
  if (originals.has(element) || !element.style) return;
  const values = {};
  for (const property of OVERLAY_STYLE_PROPERTIES) {
    values[property] = {
      value: element.style.getPropertyValue(property),
      priority: element.style.getPropertyPriority(property)
    };
  }
  originals.set(element, values);
}

function setImportantStyle(element, property, value) {
  if (!element.style) return;
  if (element.style.getPropertyValue(property) === value
      && element.style.getPropertyPriority(property) === "important") return;
  element.style.setProperty(property, value, "important");
}

function restoreOverlay(element, originals) {
  const values = originals.get(element);
  if (values && element.style) {
    for (const property of OVERLAY_STYLE_PROPERTIES) {
      const original = values[property];
      if (original.value) element.style.setProperty(property, original.value, original.priority);
      else element.style.removeProperty(property);
    }
  }
  if (element.classList) element.classList.remove("sdsync-overlay-bounded");
  originals.delete(element);
}

function boundOverlay(overlay, shell, originals) {
  rememberInlineStyles(overlay, originals);
  if (!overlay.classList.contains("sdsync-overlay-bounded")) {
    overlay.classList.add("sdsync-overlay-bounded");
  }
  const shellRect = shell.getBoundingClientRect();
  const availableWidth = Math.max(1, shellRect.width - (BOUNDARY_INSET * 2));
  const availableHeight = Math.max(1, shellRect.height - (BOUNDARY_INSET * 2));
  const boundedWidth = Math.min(360, availableWidth);
  setImportantStyle(overlay, "width", `${boundedWidth}px`);
  setImportantStyle(overlay, "max-width", `${boundedWidth}px`);
  setImportantStyle(overlay, "max-height", `${Math.min(420, availableHeight)}px`);

  const overlayRect = overlay.getBoundingClientRect();
  const desiredLeft = clamp(
    overlayRect.left,
    shellRect.left + BOUNDARY_INSET,
    shellRect.right - BOUNDARY_INSET - overlayRect.width
  );
  const desiredTop = clamp(
    overlayRect.top,
    shellRect.top + BOUNDARY_INSET,
    shellRect.bottom - BOUNDARY_INSET - overlayRect.height
  );
  setImportantStyle(overlay, "position", "fixed");
  setImportantStyle(overlay, "left", `${desiredLeft}px`);
  setImportantStyle(overlay, "right", "auto");
  setImportantStyle(overlay, "top", `${desiredTop}px`);
  setImportantStyle(overlay, "bottom", "auto");
}

function clearFieldTip(content) {
  if (!content || !content.style) return;
  content.classList.remove("sdsync-tip-bounded");
  content.style.removeProperty("--sdsync-tip-left");
  content.style.removeProperty("--sdsync-tip-top");
  content.style.removeProperty("--sdsync-tip-max-width");
  content.style.removeProperty("--sdsync-tip-max-height");
}

function shellContaining(element, shells) {
  for (const shell of shells) {
    if (shell === element || (typeof shell.contains === "function" && shell.contains(element))) {
      return shell;
    }
  }
  return null;
}

function boundFieldTip(owner, content, shell) {
  const shellRect = shell.getBoundingClientRect();
  const ownerRect = owner.getBoundingClientRect();
  const maximumWidth = Math.max(1, Math.min(240, shellRect.width - (BOUNDARY_INSET * 2)));
  const maximumHeight = Math.max(1, Math.min(180, shellRect.height - (BOUNDARY_INSET * 2)));
  content.classList.add("sdsync-tip-bounded");
  content.style.setProperty("--sdsync-tip-left", "0px");
  content.style.setProperty("--sdsync-tip-top", `${ownerRect.height + 5}px`);
  content.style.setProperty("--sdsync-tip-max-width", `${maximumWidth}px`);
  content.style.setProperty("--sdsync-tip-max-height", `${maximumHeight}px`);

  const contentRect = content.getBoundingClientRect();
  const desiredLeft = clamp(
    ownerRect.left,
    shellRect.left + BOUNDARY_INSET,
    shellRect.right - BOUNDARY_INSET - contentRect.width
  );
  const belowTop = ownerRect.bottom + 5;
  const aboveTop = ownerRect.top - 5 - contentRect.height;
  let desiredTop = belowTop;
  if (belowTop + contentRect.height > shellRect.bottom - BOUNDARY_INSET
      && aboveTop >= shellRect.top + BOUNDARY_INSET) {
    desiredTop = aboveTop;
  } else {
    desiredTop = clamp(
      belowTop,
      shellRect.top + BOUNDARY_INSET,
      shellRect.bottom - BOUNDARY_INSET - contentRect.height
    );
  }
  content.style.setProperty("--sdsync-tip-left", `${desiredLeft - ownerRect.left}px`);
  content.style.setProperty("--sdsync-tip-top", `${desiredTop - ownerRect.top}px`);
}

function hasOwnedOverlay(node) {
  if (!node || node.nodeType !== 1) return false;
  if (typeof node.matches === "function" && node.matches(OWNED_OVERLAY_SELECTOR)) return true;
  return typeof node.querySelector === "function" && Boolean(node.querySelector(OWNED_OVERLAY_SELECTOR));
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
  const observedShells = new Set();
  const observedOverlays = new Set();
  const boundedTips = new Set();
  const overlayOriginalStyles = new Map();
  const overlayFrameIds = new Set();
  let refreshQueued = false;

  const applyBoundaries = () => {
    if (!active) return;
    const shells = visibleShells(observedShells);
    const currentTips = new Set();
    for (const owner of matchingElements(root, FIELD_TIP_SELECTOR)) {
      const content = owner.querySelector(".sdsync-field-tip-content");
      const shell = content ? shellContaining(owner, shells) : null;
      if (!content || !shell) continue;
      boundFieldTip(owner, content, shell);
      currentTips.add(content);
      boundedTips.add(content);
    }
    for (const content of Array.from(boundedTips)) {
      if (currentTips.has(content)) continue;
      clearFieldTip(content);
      boundedTips.delete(content);
    }

    if (typeof document === "undefined" || !document.querySelectorAll) return;
    const overlays = new Set(document.querySelectorAll(OWNED_OVERLAY_SELECTOR));
    for (const overlay of overlays) {
      const shell = nearestShell(overlay, shells);
      if (shell) boundOverlay(overlay, shell, overlayOriginalStyles);
      else restoreOverlay(overlay, overlayOriginalStyles);
      if (resizeObserver && !observedOverlays.has(overlay)) {
        observedOverlays.add(overlay);
        resizeObserver.observe(overlay);
      }
    }
    for (const overlay of Array.from(overlayOriginalStyles.keys())) {
      if (overlays.has(overlay)) continue;
      if (resizeObserver && observedOverlays.has(overlay)) resizeObserver.unobserve(overlay);
      observedOverlays.delete(overlay);
      restoreOverlay(overlay, overlayOriginalStyles);
    }
  };

  const queueRefresh = () => {
    if (!active || refreshQueued) return;
    refreshQueued = true;
    Promise.resolve().then(() => {
      refreshQueued = false;
      if (!active) return;
      refresh();
    });
  };

  const settleOwnedOverlays = () => {
    queueRefresh();
    if (typeof window === "undefined" || typeof window.requestAnimationFrame !== "function") return;
    const first = window.requestAnimationFrame(() => {
      overlayFrameIds.delete(first);
      if (!active) return;
      const second = window.requestAnimationFrame(() => {
        overlayFrameIds.delete(second);
        if (active) queueRefresh();
      });
      overlayFrameIds.add(second);
    });
    overlayFrameIds.add(first);
  };

  const resizeObserver = typeof ResizeObserver === "function"
    ? new ResizeObserver((entries) => {
      if (!active) return;
      for (const entry of entries) {
        if (observedShells.has(entry.target)) setShellState(entry.target);
        else if (observedForms.has(entry.target)) setCompactState(entry.target);
      }
      applyBoundaries();
    })
    : null;

  const refresh = () => {
    if (!active) return;
    markControlShells(root);
    markCheckboxParts(root);
    markSelectParts(root);
    markFormControlPaths(root);
    const shells = new Set(matchingElements(root, APP_SHELL_SELECTOR));
    if (resizeObserver) {
      for (const shell of observedShells) {
        if (shells.has(shell)) continue;
        resizeObserver.unobserve(shell);
        observedShells.delete(shell);
        clearShellState(shell);
      }
    }
    for (const shell of shells) {
      setShellState(shell);
      if (!observedShells.has(shell)) {
        observedShells.add(shell);
        if (resizeObserver) resizeObserver.observe(shell);
      }
    }
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
    applyBoundaries();
  };

  const mutationObserver = typeof MutationObserver === "function"
    ? new MutationObserver(queueRefresh)
    : null;
  if (mutationObserver) mutationObserver.observe(root, { childList: true, subtree: true });

  const overlayMutationObserver = typeof MutationObserver === "function"
      && typeof document !== "undefined" && document.body
    ? new MutationObserver((records) => {
      if (!active) return;
      const relevant = records.some((record) => {
        return Array.from(record.addedNodes || []).some(hasOwnedOverlay)
          || Array.from(record.removedNodes || []).some(hasOwnedOverlay);
      });
      if (relevant) settleOwnedOverlays();
    })
    : null;
  if (overlayMutationObserver) {
    overlayMutationObserver.observe(document.body, {
      childList: true,
      subtree: true
    });
  }

  const onWindowGeometry = () => queueRefresh();
  if (typeof window !== "undefined") {
    window.addEventListener("resize", onWindowGeometry);
    window.addEventListener("scroll", onWindowGeometry, true);
  }
  refresh();

  return () => {
    if (!active) return;
    active = false;
    if (mutationObserver) mutationObserver.disconnect();
    if (overlayMutationObserver) overlayMutationObserver.disconnect();
    if (resizeObserver) resizeObserver.disconnect();
    if (typeof window !== "undefined") {
      window.removeEventListener("resize", onWindowGeometry);
      window.removeEventListener("scroll", onWindowGeometry, true);
      if (typeof window.cancelAnimationFrame === "function") {
        for (const frame of overlayFrameIds) window.cancelAnimationFrame(frame);
      }
    }
    for (const form of observedForms) form.classList.remove("sdsync-compact-form");
    for (const shell of observedShells) clearShellState(shell);
    for (const content of boundedTips) clearFieldTip(content);
    for (const overlay of Array.from(overlayOriginalStyles.keys())) {
      restoreOverlay(overlay, overlayOriginalStyles);
    }
    observedForms.clear();
    observedShells.clear();
    observedOverlays.clear();
    overlayFrameIds.clear();
    boundedTips.clear();
  };
}
