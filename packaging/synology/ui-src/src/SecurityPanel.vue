<template>
  <v-form
    :value="value"
    class="sdsync-security-form"
    direction="vertical"
    @submit="submit"
  >
    <div class="sdsync-security-grid">
      <section class="sdsync-panel">
        <div class="sdsync-panel-heading">
          <div><p class="sdsync-eyebrow">Change permissions</p><h3>Dashboard operations</h3></div>
        </div>
        <div class="sdsync-policy-list">
          <div v-for="control in operationControls" :key="control.key" class="sdsync-policy-control">
            <v-checkbox
              :value="value[control.key] === true"
              :disabled="disabled"
              :aria-describedby="helpId(control.key)"
              @input="updateField(control.key, $event === true)"
            >{{ control.label }}</v-checkbox>
            <policy-help :help-key="control.key" />
          </div>
        </div>
      </section>

      <section class="sdsync-panel">
        <div class="sdsync-panel-heading">
          <div><p class="sdsync-eyebrow">Risk ceilings</p><h3>Allowed profile behavior</h3></div>
        </div>
        <div class="sdsync-policy-list">
          <div v-for="control in riskControls" :key="control.key" class="sdsync-policy-control">
            <v-checkbox
              :value="value[control.key] === true"
              :disabled="disabled"
              :aria-describedby="helpId(control.key)"
              @input="updateField(control.key, $event === true)"
            >{{ control.label }}</v-checkbox>
            <policy-help :help-key="control.key" />
          </div>
        </div>
      </section>
    </div>

    <section class="sdsync-panel">
      <div class="sdsync-panel-heading">
        <div><p class="sdsync-eyebrow">Bounded resources</p><h3>Request and result limits</h3></div>
      </div>
      <div class="sdsync-form-grid">
        <v-form-item label="Policy version">
          <v-input
            :value="policyVersionLabel"
            readonly
            aria-describedby="sdsync-help-security-policy_version"
          />
          <policy-help help-key="policy_version" />
        </v-form-item>
        <v-form-item label="CSRF lifetime (seconds)">
          <v-input
            :value="value.csrf_lifetime_seconds"
            number-only
            :disabled="disabled"
            :aria-describedby="helpId('csrf_lifetime_seconds')"
            @input="updateField('csrf_lifetime_seconds', $event)"
          />
          <policy-help help-key="csrf_lifetime_seconds" />
        </v-form-item>
        <v-form-item label="Result retention (seconds)">
          <v-input
            :value="value.result_retention_seconds"
            number-only
            :disabled="disabled"
            :aria-describedby="helpId('result_retention_seconds')"
            @input="updateField('result_retention_seconds', $event)"
          />
          <policy-help help-key="result_retention_seconds" />
        </v-form-item>
        <v-form-item label="Maximum outstanding jobs">
          <v-input
            :value="value.max_outstanding_jobs"
            number-only
            :disabled="disabled"
            :aria-describedby="helpId('max_outstanding_jobs')"
            @input="updateField('max_outstanding_jobs', $event)"
          />
          <policy-help help-key="max_outstanding_jobs" />
        </v-form-item>
      </div>
    </section>

    <section class="sdsync-panel">
      <div class="sdsync-panel-heading">
        <div><p class="sdsync-eyebrow">Structured observability</p><h3>Log category levels</h3></div>
      </div>
      <div class="sdsync-log-policy-grid">
        <v-form-item v-for="category in logCategories" :key="category.key" :label="category.label">
          <v-single-select
            :value="value.log_levels && value.log_levels[category.key]"
            :options="logLevelOptions"
            width="100%"
            :disabled="disabled"
            :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass"
            :aria-describedby="helpId('log-' + category.key)"
            @input="updateLogLevel(category.key, $event)"
          />
          <policy-help :help-key="'log-' + category.key" />
        </v-form-item>
      </div>
    </section>

    <div class="sdsync-security-actions">
      <span class="sdsync-field-note">Changes apply to new dashboard requests and package operations.</span>
      <span v-if="dirty" class="sdsync-field-note" role="status">Unsaved security changes</span>
      <v-button
        suffix="main"
        html-type="submit"
        tooltip="Validate, persist, enforce, and audit this security and logging policy"
        :disabled="disabled || busy || !dirty"
      >Save security policy</v-button>
    </div>
  </v-form>
</template>

<script>
const POLICY_HELP = Object.freeze({
  policy_version: "Immutable on-disk security policy schema reported by the package snapshot; updates are managed only by package migrations.",
  require_https: "Reject dashboard requests unless DSM reports an HTTPS connection.",
  allow_interface_changes: "Allow this AppWindow to save and audit per-browser interface preferences.",
  allow_profile_changes: "Allow profile creation, updates, removal, and default-profile selection.",
  allow_secret_changes: "Allow replacing or clearing package-protected password, TOTP, and log-token material.",
  allow_routine_changes: "Allow routine and schedule configuration changes.",
  allow_notification_changes: "Allow package alert policy and open-session notification preference changes.",
  allow_operational_actions: "Allow manual plan, sync, and Doctor operations from the dashboard.",
  allow_http_targets: "Permit profiles that explicitly opt into unencrypted HTTP destination URLs.",
  allow_empty_source: "Permit profiles to disable the empty-source deletion guard; keep this off unless the source may intentionally be empty.",
  allow_invalid_tls: "Permit profiles that explicitly opt out of TLS certificate validation.",
  allow_destructive_sync: "Permit profiles, routines, or runs to request bounded destination deletion.",
  allow_doctor_write_test: "Permit Doctor to create, verify, and remove a disposable destination probe.",
  allow_remote_logging: "Permit profiles to deliver bounded structured logs to an HTTPS collector.",
  csrf_lifetime_seconds: "Validity of a session-bound dashboard mutation token; accepted range is 60 through 900 seconds.",
  result_retention_seconds: "How long completed queued results remain observable; accepted range is 300 through 86400 seconds.",
  max_outstanding_jobs: "Caps queued plus processing jobs at N (1 through 256). Retained terminal responses are capped separately at the same N, so the worst case is 2N private job records.",
  "log-audit": "Optional audit verbosity. Mandatory action records remain at info even when this is off.",
  "log-bridge": "Minimum level for accepted and rejected dashboard mutation bridge events emitted by the package.",
  "log-authentication": "Minimum level for authenticated identity events emitted for accepted dashboard mutations.",
  "log-security": "Minimum level for emitted security-policy changes and policy or security mutation rejections.",
  "log-configuration": "Minimum level for profile, schedule, and package configuration events.",
  "log-secrets": "Minimum level for secret-presence operations; secret values are never logged.",
  "log-routines": "Minimum level for routine scheduling, deferral, retry, and dependency events.",
  "log-operations": "Minimum level for manual plan, sync, and Doctor operation events.",
  "log-notifications": "Minimum level for DSM alert delivery and suppression events.",
  "log-sync": "Minimum level for structured sync-engine output.",
  "log-controller": "Minimum level for emitted controller queue-processing and lifecycle diagnostics.",
  "log-scheduler": "Minimum level for scheduler wake, dispatch, and deferral output."
});

const PolicyHelp = {
  name: "PolicyHelp",
  props: { helpKey: { type: String, required: true } },
  computed: {
    helpId() { return `sdsync-help-security-${this.helpKey}`; },
    text() { return POLICY_HELP[this.helpKey] || "See the Security section in DSM Help."; }
  },
  template: `<span class="sdsync-field-tip"><button type="button" class="sdsync-field-tip-trigger" :title="text" :aria-label="'Help: ' + text" :aria-describedby="helpId" @keydown.esc="$event.currentTarget.blur()">?</button><span :id="helpId" class="sdsync-field-tip-content" role="tooltip">{{ text }}</span></span>`
};

export default {
  name: "SecurityPanel",
  components: { PolicyHelp },
  props: {
    value: { type: Object, required: true },
    disabled: { type: Boolean, default: false },
    busy: { type: Boolean, default: false },
    dirty: { type: Boolean, default: false },
    themeClass: { type: String, default: "is-dark" },
    logLevelOptions: { type: Array, required: true }
  },
  data() {
    return {
      operationControls: [
        { key: "require_https", label: "Require HTTPS for this DSM dashboard" },
        { key: "allow_interface_changes", label: "Allow interface preference changes" },
        { key: "allow_profile_changes", label: "Allow profile configuration changes" },
        { key: "allow_secret_changes", label: "Allow protected-secret changes" },
        { key: "allow_routine_changes", label: "Allow routine and schedule changes" },
        { key: "allow_notification_changes", label: "Allow notification policy changes" },
        { key: "allow_operational_actions", label: "Allow manual plan, sync, and Doctor actions" }
      ],
      riskControls: [
        { key: "allow_http_targets", label: "Allow profile-level HTTP exceptions" },
        { key: "allow_empty_source", label: "Allow profile-level empty-source exceptions" },
        { key: "allow_invalid_tls", label: "Allow profile-level invalid-certificate exceptions" },
        { key: "allow_destructive_sync", label: "Allow deletion-capable profiles and runs" },
        { key: "allow_doctor_write_test", label: "Allow disposable Doctor write tests" },
        { key: "allow_remote_logging", label: "Allow HTTPS remote log delivery" }
      ],
      logCategories: [
        { key: "audit", label: "Audit" },
        { key: "bridge", label: "Bridge" },
        { key: "authentication", label: "Authentication" },
        { key: "security", label: "Security" },
        { key: "configuration", label: "Configuration" },
        { key: "secrets", label: "Secrets" },
        { key: "routines", label: "Routines" },
        { key: "operations", label: "Operations" },
        { key: "notifications", label: "Notifications" },
        { key: "sync", label: "Sync" },
        { key: "controller", label: "Controller" },
        { key: "scheduler", label: "Scheduler" }
      ]
    };
  },
  computed: {
    policyVersionLabel() {
      return Number.isSafeInteger(this.value.policy_version) && this.value.policy_version > 0
        ? String(this.value.policy_version)
        : "Unavailable";
    }
  },
  methods: {
    helpId(key) { return `sdsync-help-security-${key}`; },
    updateField(key, value) {
      this.$emit("input", Object.assign({}, this.value, { [key]: value }));
    },
    updateLogLevel(key, level) {
      const logLevels = Object.assign({}, this.value.log_levels || {}, { [key]: level });
      this.$emit("input", Object.assign({}, this.value, { log_levels: logLevels }));
    },
    submit(event) {
      if (event && event.preventDefault) event.preventDefault();
      this.$emit("save", event);
    }
  }
};
</script>
