export const AUTOSAVE_DELAY_MS = 1300;

function canonicalValue(value, ancestors) {
  if (value === null || typeof value === "string" || typeof value === "boolean") return value;
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new TypeError("Autosave values must contain only finite numbers");
    return Object.is(value, -0) ? 0 : value;
  }
  if (typeof value !== "object") {
    throw new TypeError("Autosave values must be JSON-compatible");
  }
  if (ancestors.has(value)) throw new TypeError("Autosave values must not contain cycles");
  if (Object.getOwnPropertySymbols(value).length) {
    throw new TypeError("Autosave values must not contain symbol keys");
  }

  ancestors.add(value);
  let normalized;
  if (Array.isArray(value)) {
    normalized = [];
    for (let index = 0; index < value.length; index += 1) {
      normalized.push(canonicalValue(value[index], ancestors));
    }
  } else {
    const prototype = Object.getPrototypeOf(value);
    if (prototype !== Object.prototype && prototype !== null) {
      ancestors.delete(value);
      throw new TypeError("Autosave values must use plain objects");
    }
    normalized = Object.create(null);
    Object.keys(value).sort().forEach((key) => {
      normalized[key] = canonicalValue(value[key], ancestors);
    });
  }
  ancestors.delete(value);
  return normalized;
}

export function canonicalAutosaveSignature(value) {
  return JSON.stringify(canonicalValue(value, new WeakSet()));
}

function capturedValue(value) {
  const signature = canonicalAutosaveSignature(value);
  return { signature, value: JSON.parse(signature) };
}

function validScope(scope) {
  return typeof scope === "string"
    && scope.length > 0
    && scope.length <= 256
    && scope.trim() === scope
    && !/[\0-\x1f\x7f]/.test(scope);
}

function requireScope(scope) {
  if (!validScope(scope)) throw new TypeError("Autosave scope must be a bounded non-empty string");
  return scope;
}

function validBoolean(value) {
  if (typeof value !== "boolean") throw new TypeError("Autosave state flags must be boolean");
  return value;
}

export class AutosaveCoordinator {
  constructor(options) {
    const configuration = options || {};
    if (typeof configuration.dispatch !== "function") {
      throw new TypeError("Autosave dispatch must be a function");
    }
    const delay = configuration.delayMs === undefined
      ? AUTOSAVE_DELAY_MS
      : Number(configuration.delayMs);
    if (!Number.isInteger(delay) || delay < 0 || delay > 60000) {
      throw new TypeError("Autosave delay must be an integer from 0 through 60000 milliseconds");
    }

    this.delayMs = delay;
    this.dispatch = configuration.dispatch;
    this.onError = typeof configuration.onError === "function" ? configuration.onError : null;
    this.onSuccess = typeof configuration.onSuccess === "function" ? configuration.onSuccess : null;
    this.now = typeof configuration.now === "function" ? configuration.now : () => Date.now();
    this.setTimer = typeof configuration.setTimeout === "function"
      ? configuration.setTimeout
      : (callback, milliseconds) => setTimeout(callback, milliseconds);
    this.clearTimer = typeof configuration.clearTimeout === "function"
      ? configuration.clearTimeout
      : (timer) => clearTimeout(timer);
    this.entries = new Map();
    this.queue = [];
    this.activeScope = "";
    this.globalBusy = false;
    this.disposed = false;
  }

  _assertActive() {
    if (this.disposed) throw new Error("Autosave coordinator is disposed");
  }

  _time() {
    const value = Number(this.now());
    if (!Number.isFinite(value) || value < 0) throw new Error("Autosave clock returned an invalid value");
    return value;
  }

  _entry(scope) {
    const name = requireScope(scope);
    let entry = this.entries.get(name);
    if (!entry) {
      entry = {
        scope: name,
        baselineSignature: null,
        signature: null,
        value: null,
        revision: 0,
        epoch: 0,
        lastTouchedAt: null,
        dueAt: 0,
        timer: null,
        queued: false,
        busy: false,
        blocked: false,
        cancelled: false,
        inFlight: false,
        inFlightSignature: null,
        inFlightEpoch: 0
      };
      this.entries.set(name, entry);
    }
    return entry;
  }

  _effectiveBaseline(entry) {
    if (entry.inFlight && entry.inFlightEpoch === entry.epoch) return entry.inFlightSignature;
    return entry.baselineSignature;
  }

  _dirty(entry) {
    return entry.signature !== null && entry.signature !== this._effectiveBaseline(entry);
  }

  _removeQueued(entry) {
    if (!entry.queued) return;
    entry.queued = false;
    this.queue = this.queue.filter((scope) => scope !== entry.scope);
  }

  _clearPending(entry) {
    if (entry.timer !== null) {
      this.clearTimer(entry.timer);
      entry.timer = null;
    }
    this._removeQueued(entry);
  }

  _enqueue(entry) {
    if (this.disposed || entry.cancelled || !this._dirty(entry) || entry.queued) return;
    entry.queued = true;
    this.queue.push(entry.scope);
    this._drain();
  }

  _arm(entry) {
    if (this.disposed || entry.cancelled || entry.lastTouchedAt === null || !this._dirty(entry)) {
      this._clearPending(entry);
      if (!this._dirty(entry)) {
        entry.lastTouchedAt = null;
        entry.dueAt = 0;
      }
      return;
    }
    if (entry.timer !== null || entry.queued) return;
    const remaining = Math.max(0, entry.dueAt - this._time());
    if (remaining === 0) {
      this._enqueue(entry);
      return;
    }
    entry.timer = this.setTimer(() => {
      entry.timer = null;
      if (!this.disposed) this._enqueue(entry);
    }, remaining);
  }

  _nextEligibleEntry() {
    for (let index = 0; index < this.queue.length;) {
      const scope = this.queue[index];
      const entry = this.entries.get(scope);
      if (!entry || !entry.queued || entry.cancelled || !this._dirty(entry)) {
        this.queue.splice(index, 1);
        if (entry) entry.queued = false;
        continue;
      }
      if (entry.dueAt > this._time()) {
        this.queue.splice(index, 1);
        entry.queued = false;
        this._arm(entry);
        continue;
      }
      if (entry.busy || entry.blocked) {
        index += 1;
        continue;
      }
      this.queue.splice(index, 1);
      entry.queued = false;
      return entry;
    }
    return null;
  }

  _notify(callback, first, second) {
    if (!callback || this.disposed) return;
    try {
      callback(first, second);
    } catch (_error) {
      // Status callbacks must never break queue serialization.
    }
  }

  _finishDispatch(entry, task, succeeded, error) {
    const currentEpoch = task.epoch === entry.epoch;
    if (this.activeScope === entry.scope) this.activeScope = "";
    entry.inFlight = false;
    entry.inFlightSignature = null;
    entry.inFlightEpoch = 0;

    if (succeeded && currentEpoch) {
      entry.baselineSignature = task.signature;
    } else if (!succeeded && currentEpoch && !this.disposed) {
      // Never retry a rejected or outcome-unknown mutation merely because its
      // original debounce has elapsed. Integration must explicitly reconcile
      // and unblock the scope.
      entry.blocked = true;
    }

    if (!this.disposed && !entry.cancelled) this._arm(entry);
    // Hydration and baseline replacement are authoritative reconciliation
    // boundaries. A completion from an older epoch must not overwrite their
    // status or pause later edits after that reconciliation has taken place.
    if (currentEpoch) {
      if (succeeded) this._notify(this.onSuccess, task);
      else this._notify(this.onError, error, task);
    }
    if (!this.disposed) this._drain();
  }

  _startDispatch(entry) {
    const task = Object.freeze({
      scope: entry.scope,
      value: JSON.parse(entry.signature),
      signature: entry.signature,
      revision: entry.revision,
      epoch: entry.epoch
    });
    this.activeScope = entry.scope;
    entry.inFlight = true;
    entry.inFlightSignature = task.signature;
    entry.inFlightEpoch = task.epoch;

    let result;
    try {
      result = this.dispatch(task);
    } catch (error) {
      this._finishDispatch(entry, task, false, error);
      return;
    }
    Promise.resolve(result).then(
      () => this._finishDispatch(entry, task, true, null),
      (error) => this._finishDispatch(entry, task, false, error)
    );
  }

  _drain() {
    if (this.disposed || this.globalBusy || this.activeScope) return;
    const entry = this._nextEligibleEntry();
    if (entry) this._startDispatch(entry);
  }

  hydrate(scope, value) {
    this._assertActive();
    const entry = this._entry(scope);
    const captured = capturedValue(value);
    this._clearPending(entry);
    entry.epoch += 1;
    entry.revision += 1;
    entry.baselineSignature = captured.signature;
    entry.signature = captured.signature;
    entry.value = captured.value;
    entry.lastTouchedAt = null;
    entry.dueAt = 0;
    entry.blocked = false;
    entry.cancelled = false;
    return this.getState(entry.scope);
  }

  replaceBaseline(scope, value) {
    this._assertActive();
    const entry = this._entry(scope);
    const captured = capturedValue(value);
    entry.epoch += 1;
    entry.baselineSignature = captured.signature;
    if (entry.signature === null) {
      entry.signature = captured.signature;
      entry.value = captured.value;
      entry.revision += 1;
    }
    if (!this._dirty(entry)) {
      this._clearPending(entry);
      entry.lastTouchedAt = null;
      entry.dueAt = 0;
    }
    else this._arm(entry);
    return this.getState(entry.scope);
  }

  update(scope, value) {
    this._assertActive();
    const entry = this._entry(scope);
    if (entry.baselineSignature === null) {
      throw new Error("Autosave scope must be hydrated before it can be updated");
    }
    const captured = capturedValue(value);
    if (captured.signature === entry.signature && !entry.cancelled) {
      return this.getState(entry.scope);
    }

    this._clearPending(entry);
    entry.revision += 1;
    entry.signature = captured.signature;
    entry.value = captured.value;
    entry.lastTouchedAt = this._time();
    entry.dueAt = entry.lastTouchedAt + this.delayMs;
    entry.cancelled = false;
    this._arm(entry);
    return this.getState(entry.scope);
  }

  setScopeBusy(scope, busy) {
    this._assertActive();
    const entry = this._entry(scope);
    entry.busy = validBoolean(busy);
    if (!entry.busy) {
      this._arm(entry);
      this._drain();
    }
    return this.getState(entry.scope);
  }

  setScopeBlocked(scope, blocked) {
    this._assertActive();
    const entry = this._entry(scope);
    entry.blocked = validBoolean(blocked);
    if (!entry.blocked) {
      this._arm(entry);
      this._drain();
    }
    return this.getState(entry.scope);
  }

  setGlobalBusy(busy) {
    this._assertActive();
    this.globalBusy = validBoolean(busy);
    if (!this.globalBusy) this._drain();
  }

  cancel(scope) {
    if (this.disposed) return false;
    const name = requireScope(scope);
    const entry = this.entries.get(name);
    if (!entry) return false;
    this._clearPending(entry);
    entry.cancelled = true;
    entry.dueAt = 0;
    return true;
  }

  cancelAll() {
    if (this.disposed) return 0;
    let cancelled = 0;
    this.entries.forEach((entry) => {
      if (entry.timer !== null || entry.queued || this._dirty(entry)) cancelled += 1;
      this._clearPending(entry);
      entry.cancelled = true;
      entry.dueAt = 0;
    });
    return cancelled;
  }

  getState(scope) {
    const name = requireScope(scope);
    const entry = this.entries.get(name);
    if (!entry) {
      return Object.freeze({
        scope: name,
        registered: false,
        dirty: false,
        scheduled: false,
        queued: false,
        busy: false,
        blocked: false,
        cancelled: false,
        inFlight: false,
        revision: 0,
        dueAt: 0
      });
    }
    return Object.freeze({
      scope: name,
      registered: entry.baselineSignature !== null,
      dirty: this._dirty(entry),
      scheduled: entry.timer !== null,
      queued: entry.queued,
      busy: entry.busy,
      blocked: entry.blocked,
      cancelled: entry.cancelled,
      inFlight: entry.inFlight,
      revision: entry.revision,
      dueAt: entry.dueAt
    });
  }

  dispose() {
    if (this.disposed) return;
    this.entries.forEach((entry) => {
      this._clearPending(entry);
      entry.cancelled = true;
    });
    this.queue = [];
    this.disposed = true;
  }
}

export function createAutosaveCoordinator(options) {
  return new AutosaveCoordinator(options);
}
