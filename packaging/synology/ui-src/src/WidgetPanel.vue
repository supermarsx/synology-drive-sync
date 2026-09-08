<template>
  <div class="sdsync-widget" :class="rootClasses" role="group" aria-label="Synology Drive Sync status">
    <div class="sdsync-widget-headline">
      <span class="sdsync-widget-dot" aria-hidden="true" />
      <div class="sdsync-widget-headline-text">
        <strong class="sdsync-widget-title">{{ headline.title }}</strong>
        <span class="sdsync-widget-detail">{{ headline.detail }}</span>
      </div>
    </div>
    <!-- No aria-live role: this card sits on the DSM desktop for the whole
         session and must not announce package state unprompted. The AppWindow
         owns the live regions for operations the operator actually started. -->
    <p v-if="run.active" class="sdsync-widget-run">
      <action-icon name="run" :size="12" />
      <span>{{ run.label }}</span>
    </p>
    <ul v-if="!compact && rows.length" class="sdsync-widget-profiles" aria-label="Per-profile sync outcome">
      <li v-for="row in rows" :key="row.name" class="sdsync-widget-profile" :class="'is-' + row.level" :title="row.name + ' — ' + row.outcome + ', ' + row.detail">
        <span class="sdsync-widget-profile-name">{{ row.name }}</span>
        <span class="sdsync-widget-profile-outcome">{{ row.outcome }}</span>
        <span class="sdsync-widget-profile-age">{{ row.age }}</span>
      </li>
    </ul>
    <p class="sdsync-widget-footer">
      <span>{{ freshness }}</span>
      <span v-if="degraded" class="sdsync-widget-degraded">Retrying slowly</span>
    </p>
  </div>
</template>

<script>
import { ActionIcon } from "./ActionIcon";
import { SNAPSHOT_SCHEMA, apiGet } from "./api";
import {
  APPWINDOW_SETTINGS_KEY,
  appWindowPreferences,
  widgetBridgeIssue,
  widgetOverview,
  widgetPollDelay,
  widgetProfileRows,
  widgetRun
} from "./widgetModel.mjs";

// Two consecutive failures before the card admits it is degraded. One failed
// read during a DSM session renewal is normal and the retained snapshot is
// still the best answer available; saying "Retrying slowly" for it would train
// operators to ignore the message when it finally means something.
const DEGRADED_AFTER_FAILURES = 2;

export default {
  name: "WidgetPanel",
  components: { ActionIcon },
  data() {
    return {
      compact: true,
      snapshot: null,
      bridgeIssue: null,
      failures: 0,
      loading: false,
      activated: false,
      disposed: false,
      documentHidden: false,
      updatedAtMs: 0,
      nowMs: Date.now(),
      preferences: appWindowPreferences(null),
      systemLight: false
    };
  },
  computed: {
    overview() { return widgetOverview(this.snapshot, this.nowMs); },
    // What the card actually leads with. A bridge failure takes the headline as
    // soon as there is nothing better to show, and once it is clearly not a
    // blip; in between, a single failed read leaves the last good status up
    // rather than flapping the card. Retained rows stay visible underneath
    // either way, with the footer carrying their age.
    headline() {
      return this.bridgeIssue && (this.degraded || !this.snapshot)
        ? this.bridgeIssue
        : this.overview;
    },
    rows() { return widgetProfileRows(this.snapshot, this.nowMs); },
    run() { return widgetRun(this.snapshot); },
    degraded() { return this.failures >= DEGRADED_AFTER_FAILURES; },
    themeClass() {
      const theme = this.preferences.theme === "system"
        ? (this.systemLight ? "is-light" : "is-dark")
        : `is-${this.preferences.theme}`;
      return theme;
    },
    rootClasses() {
      return [
        this.themeClass,
        `is-${this.headline.level}`,
        { "is-compact": this.compact }
      ];
    },
    freshness() {
      if (!this.updatedAtMs) return this.loading ? "Reading package status…" : "Not read yet";
      try {
        const stamp = new Intl.DateTimeFormat(undefined, { timeStyle: "short" })
          .format(new Date(this.updatedAtMs));
        return `Updated ${stamp}`;
      } catch (_error) {
        return "Updated";
      }
    }
  },
  created() {
    this.timer = 0;
    this.abortController = typeof window.AbortController === "function"
      ? new window.AbortController()
      : null;
    this.auth = { signal: this.abortController ? this.abortController.signal : undefined };
    this.readPreferences();
    this.mediaQuery = window.matchMedia ? window.matchMedia("(prefers-color-scheme: light)") : null;
    this.systemLight = Boolean(this.mediaQuery && this.mediaQuery.matches);
    this.mediaHandler = (event) => { this.systemLight = event.matches; };
    if (this.mediaQuery && this.mediaQuery.addEventListener) {
      this.mediaQuery.addEventListener("change", this.mediaHandler);
    }
    // A widget the operator can see is still worthless work when the whole DSM
    // tab is in a background window, so tab visibility gates the timer exactly
    // as it does in the AppWindow. DSM's own onActivate/onDeactivate gate the
    // separate question of whether this card is on screen at all.
    this.documentHidden = Boolean(document.hidden);
    this.visibilityHandler = () => {
      this.documentHidden = Boolean(document.hidden);
      if (this.documentHidden) this.stopTimer();
      else if (this.activated) this.refresh();
    };
    document.addEventListener("visibilitychange", this.visibilityHandler);
    this.storageHandler = (event) => {
      if (!event || event.key !== APPWINDOW_SETTINGS_KEY) return;
      this.readPreferences();
    };
    window.addEventListener("storage", this.storageHandler);
  },
  beforeDestroy() {
    this.disposed = true;
    this.activated = false;
    this.stopTimer();
    if (this.abortController) this.abortController.abort();
    document.removeEventListener("visibilitychange", this.visibilityHandler);
    window.removeEventListener("storage", this.storageHandler);
    if (this.mediaQuery && this.mediaQuery.removeEventListener && this.mediaHandler) {
      this.mediaQuery.removeEventListener("change", this.mediaHandler);
    }
  },
  methods: {
    // DSM calls the panel's onActivate/onDeactivate when the widget card
    // becomes visible or is minimised, collapsed away, or closed. The Ext
    // adapter forwards them here; nothing polls before activate().
    activate() {
      if (this.disposed || this.activated) return;
      this.activated = true;
      this.readPreferences();
      this.refresh();
    },
    deactivate() {
      if (!this.activated) return;
      this.activated = false;
      this.stopTimer();
    },
    setCompact(compact) {
      this.compact = compact !== false;
    },
    readPreferences() {
      let stored = null;
      try {
        stored = window.localStorage.getItem(APPWINDOW_SETTINGS_KEY);
      } catch (_error) {
        stored = null;
      }
      this.preferences = appWindowPreferences(stored);
    },
    stopTimer() {
      if (this.timer) window.clearTimeout(this.timer);
      this.timer = 0;
    },
    scheduleRefresh() {
      this.stopTimer();
      if (this.disposed || !this.activated || this.documentHidden) return;
      const delay = widgetPollDelay({
        active: this.run.active,
        failures: this.failures,
        appWindowIntervalMs: this.preferences.statusIntervalMs
      });
      this.timer = window.setTimeout(() => {
        this.timer = 0;
        this.refresh();
      }, delay);
    },
    async refresh() {
      if (this.disposed || !this.activated || this.documentHidden || this.loading) return;
      this.loading = true;
      try {
        const snapshot = await apiGet(this.auth, "snapshot");
        if (this.disposed) return;
        if (snapshot.schema !== SNAPSHOT_SCHEMA) throw new Error("Unsupported DSM API schema");
        this.snapshot = snapshot;
        this.bridgeIssue = null;
        this.failures = 0;
        this.updatedAtMs = Date.now();
        this.nowMs = this.updatedAtMs;
      } catch (error) {
        // The retained snapshot stays on screen. A widget that blanks itself
        // on one transport failure loses the last thing it actually knew, and
        // the footer already says the reading is no longer fresh.
        if (this.disposed) return;
        this.bridgeIssue = widgetBridgeIssue(error);
        this.failures += 1;
      } finally {
        this.loading = false;
        if (!this.disposed) this.scheduleRefresh();
      }
    }
  }
};
</script>
