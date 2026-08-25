<template>
  <v-app-instance class-name="SYNO.SDS.App.SynologyDriveSync.Instance">
    <v-app-window
      ref="appWindow"
      syno-id="SYNO.SDS.App.SynologyDriveSync.Window"
      width="1180"
      height="760"
      :resizable="true"
    >
      <div class="sdsync-app" :class="themeClass">
        <aside class="sdsync-sidebar" aria-label="Application navigation">
          <div class="sdsync-brand">
            <img src="/webman/3rdparty/synology-drive-sync/images/icon_64.png" width="42" height="42" alt="">
            <div><strong>Drive Sync</strong><span>File Station control</span></div>
          </div>
          <nav class="sdsync-nav">
            <button
              v-for="item in routes"
              :key="item.id"
              type="button"
              :class="['sdsync-nav-item', { 'is-active': route === item.id }]"
              :aria-current="route === item.id ? 'page' : null"
              :aria-label="item.title"
              :title="item.title"
              @click="navigate(item.id)"
            >
              <span class="sdsync-nav-icon" aria-hidden="true">{{ item.icon }}</span>
              <span>{{ item.title }}</span>
            </button>
          </nav>
          <div class="sdsync-sidebar-foot">
            <span :class="['sdsync-connection-dot', { 'is-online': connected, 'is-error': !connected }]" />
            <span>{{ connectionLabel }}</span>
          </div>
        </aside>

        <main class="sdsync-workspace">
          <header class="sdsync-topbar">
            <div><p class="sdsync-eyebrow">DSM package control plane</p><h1>{{ pageTitle }}</h1></div>
            <div class="sdsync-topbar-actions">
              <span class="sdsync-freshness" aria-live="polite">{{ freshness }}</span>
              <v-button
                type="round-border"
                display="text"
                tooltip="Refresh current data"
                :disabled="snapshotLoading"
                @click="refreshSnapshot(true)"
              >Refresh</v-button>
            </div>
          </header>

          <div v-if="!canMutate" class="sdsync-banner" role="status">
            <div>
              <strong>Read-only control plane</strong>
              <span>Live status remains available, but changes stay disabled until the authenticated DSM bridge is ready.</span>
            </div>
          </div>

          <section v-if="route === 'overview'" class="sdsync-page" aria-labelledby="sdsync-overview-title">
            <h2 id="sdsync-overview-title" class="sdsync-sr-only">Overview</h2>
            <div class="sdsync-hero">
              <div>
                <span :class="pillClass(serviceState)">{{ serviceState }}</span>
                <h2>Your sync estate, at a glance.</h2>
                <p>{{ overviewSummary }}</p>
              </div>
              <div class="sdsync-action-row">
                <v-button suffix="grey" :disabled="!canMutate || !profiles.length || operationBusy" @click="quickPlan">Plan all profiles</v-button>
                <v-button suffix="main" :disabled="!canMutate || !profiles.length || operationBusy" @click="quickRun">Run all profiles</v-button>
              </div>
            </div>

            <div class="sdsync-metrics" aria-label="Package summary">
              <article><span>Profiles</span><strong>{{ profiles.length }}</strong><small>{{ readyProfileCount }} with protected password material</small></article>
              <article><span>Next routine</span><strong>{{ nextRun }}</strong><small>{{ enabledRoutines.length }} enabled routine{{ enabledRoutines.length === 1 ? '' : 's' }}</small></article>
              <article><span>Last result</span><strong>{{ runStatus }}</strong><small>{{ lastRunDetail }}</small></article>
              <article><span>Active scope</span><strong>{{ runStatus === 'running' ? runScope : 'Idle' }}</strong><small>{{ runStatus === 'running' ? runOperation : 'No active operation' }}</small></article>
              <article><span>Realtime</span><strong>{{ realtimeRoutines.length ? realtimeRoutines.length + ' active' : 'Off' }}</strong><small>{{ realtimeDetail }}</small></article>
            </div>

            <div class="sdsync-two-column">
              <article class="sdsync-panel">
                <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Destinations</p><h3>Profile readiness</h3></div><v-button type="styleless" @click="navigate('profiles')">Manage profiles</v-button></div>
                <p v-if="!profiles.length" class="sdsync-empty">No configured profiles.</p>
                <div v-for="profile in profiles" :key="profile.name" class="sdsync-compact-profile">
                  <div><strong>{{ profile.name }}</strong><span>{{ profile.remote || profile.remote_path || 'Destination unavailable' }}</span></div>
                  <span>{{ profile.has_password === true ? 'Credential stored' : 'Password required' }}</span>
                </div>
              </article>
              <article class="sdsync-panel">
                <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Recent state</p><h3>Last operation</h3></div><v-button type="styleless" @click="navigate('activity')">Open activity</v-button></div>
                <dl class="sdsync-definition-grid">
                  <div><dt>Operation</dt><dd>{{ runOperation }}</dd></div><div><dt>State</dt><dd>{{ runStatus }}</dd></div>
                  <div><dt>Scope</dt><dd>{{ runScope }}</dd></div><div><dt>Started</dt><dd>{{ formatDate(run.started_epoch) }}</dd></div>
                  <div><dt>Finished</dt><dd>{{ formatDate(run.finished_epoch) }}</dd></div>
                </dl>
              </article>
            </div>
          </section>

          <section v-else-if="route === 'profiles'" class="sdsync-page" aria-labelledby="sdsync-profiles-title">
            <div class="sdsync-section-heading">
              <div><p class="sdsync-eyebrow">Configuration</p><h2 id="sdsync-profiles-title">Profiles</h2><p>Each profile owns one local source, one File Station destination, and protected credentials.</p></div>
              <v-button suffix="main" :disabled="!canMutate || operationBusy" @click="openProfile('')">New profile</v-button>
            </div>
            <div class="sdsync-profiles-layout">
              <div class="sdsync-panel sdsync-profile-catalog">
                <v-input v-model="profileFilter" clearable placeholder="Filter profiles" aria-label="Filter profiles" />
                <p v-if="!filteredProfiles.length" class="sdsync-empty">No configured profiles.</p>
                <button
                  v-for="profile in filteredProfiles"
                  :key="profile.name"
                  type="button"
                  :class="['sdsync-profile-row', { 'is-selected': selectedProfile === profile.name }]"
                  :disabled="operationBusy"
                  @click="openProfile(profile.name)"
                >
                  <span><strong>{{ profile.name }}</strong><span>{{ profile.remote || profile.remote_path || 'Destination unavailable' }}</span></span>
                  <span class="sdsync-badges"><i :class="['sdsync-mini-badge', { ready: profile.has_password === true }]">{{ profile.has_password === true ? 'Ready' : 'Needs password' }}</i><i v-if="profile.is_default === true || profile.default === true" class="sdsync-mini-badge">Default</i></span>
                </button>
              </div>

              <article v-if="!profileEditorOpen" class="sdsync-panel sdsync-editor-placeholder">
                <p class="sdsync-eyebrow">Profile editor</p><h3>Select a profile or create one</h3>
                <p>Configuration and secret operations stay separate. Stored credentials are shown only as masked presence flags.</p>
              </article>

              <v-form v-else v-model="profileForm" class="sdsync-panel sdsync-editor" direction="vertical" @submit="saveProfile">
                <div class="sdsync-panel-heading">
                  <div><p class="sdsync-eyebrow">Profile editor</p><h3>{{ selectedProfile ? 'Edit ' + selectedProfile : 'New profile' }}</h3></div>
                  <v-button type="round-border" @click="closeProfile">Close</v-button>
                </div>
                <div class="sdsync-form-grid">
                  <v-form-item label="Name" prop="name"><v-input v-model.trim="profileForm.name" :readonly="Boolean(selectedProfile)" maxlength="64" placeholder="office_nas" /></v-form-item>
                  <v-form-item label="Local source" prop="source"><v-input v-model.trim="profileForm.source" placeholder="/volume1/Source" /></v-form-item>
                  <v-form-item class="span-2" label="File Station URL" prop="url"><v-input v-model.trim="profileForm.url" placeholder="https://files.example.com" /></v-form-item>
                  <v-form-item label="DSM username" prop="username"><v-input v-model.trim="profileForm.username" autocomplete="username" /></v-form-item>
                  <v-form-item label="Remote logical path" prop="remote"><v-input v-model.trim="profileForm.remote" placeholder="/home/Drive/NAS Backup" /></v-form-item>
                  <v-form-item label="Comparison"><v-single-select v-model="profileForm.compare" :options="compareOptions" width="100%" /></v-form-item>
                  <v-form-item label="Concurrent uploads"><v-input v-model="profileForm.jobs" number-only /></v-form-item>
                  <v-checkbox v-model="profileForm.allow_http" class="span-2" :disabled="!canMutate">Allow plain HTTP for controlled LAN testing</v-checkbox>
                </div>

                <fieldset class="sdsync-danger-fieldset">
                  <legend>Deletion guard</legend>
                  <v-checkbox v-model="profileForm.delete" :disabled="!canMutate">Mirror remote deletions after profile and run-level approval</v-checkbox>
                  <v-form-item label="Maximum deletions per run"><v-input v-model="profileForm.max_delete" number-only /></v-form-item>
                </fieldset>

                <details class="sdsync-advanced">
                  <summary><strong>Advanced profile controls</strong><span>Network, retry, output, and remote observability policy</span></summary>
                  <div class="sdsync-form-grid">
                    <v-form-item class="span-2" label="Excludes"><v-input v-model="profileForm.excludes" type="textarea" :autosize="{ minRows: 3, maxRows: 7 }" placeholder="@eaDir/&#10;**/@eaDir/&#10;#recycle/" /></v-form-item>
                    <v-checkbox v-model="profileForm.allow_empty_source" class="span-2" :disabled="!canMutate">Allow an empty source (disables the empty-source deletion guard)</v-checkbox>
                    <v-form-item label="Retries"><v-input v-model="profileForm.retries" number-only /></v-form-item>
                    <v-form-item label="Upload timeout (seconds)"><v-input v-model="profileForm.timeout" number-only /></v-form-item>
                    <v-form-item label="Connect timeout (seconds)"><v-input v-model="profileForm.connect_timeout" number-only /></v-form-item>
                    <v-form-item label="Maximum rate (bytes/s)"><v-input v-model="profileForm.max_rate" number-only /></v-form-item>
                    <v-form-item class="span-2" label="CA certificate path"><v-input v-model.trim="profileForm.ca_certificate" placeholder="/volume1/certificates/ca.pem" /></v-form-item>
                    <v-checkbox v-model="profileForm.danger_invalid_certs" class="span-2" :disabled="!canMutate">Accept invalid TLS certificates (unsafe)</v-checkbox>
                    <v-checkbox v-if="profileForm.danger_invalid_certs" v-model="profileForm.danger_invalid_confirm" class="span-2" label-color="red" :disabled="!canMutate">I accept the interception risk</v-checkbox>
                    <v-form-item label="Verbosity"><v-single-select v-model="profileForm.verbosity" :options="verbosityOptions" width="100%" /></v-form-item>
                    <v-checkbox v-model="profileForm.quiet" :disabled="!canMutate">Quiet terminal sink; durable logs remain active</v-checkbox>
                    <v-form-item label="Log level"><v-single-select v-model="profileForm.log_level" :options="logLevelOptions" width="100%" /></v-form-item>
                    <v-form-item label="Log format" textonly><span>JSON · package managed</span></v-form-item>
                    <v-form-item label="Progress" textonly><span>Never · package managed</span></v-form-item>
                    <v-form-item label="Output" textonly><span>Human · package managed</span></v-form-item>
                    <v-form-item class="span-2" label="Remote log URL"><v-input v-model.trim="profileForm.remote_log_url" placeholder="https://collector.example.com/ingest" /></v-form-item>
                    <v-form-item label="Remote log mode"><v-single-select v-model="profileForm.remote_log_mode" :options="remoteLogModeOptions" width="100%" /></v-form-item>
                  </div>
                  <div class="sdsync-secret-editor">
                    <div><strong>Remote log token</strong><span>{{ selectedProfileModel && selectedProfileModel.has_remote_log_token ? 'Stored · masked' : 'Not stored' }}</span></div>
                    <v-single-select v-model="secretModes.remote_log_token" :options="secretModeOptions" width="210" :disabled="!canManageSecrets" />
                    <v-input v-if="secretModes.remote_log_token === 'replace'" v-model="secretValues.remote_log_token" type="password" maxlength="4096" autocomplete="new-password" placeholder="New token" />
                  </div>
                </details>

                <fieldset class="sdsync-secret-fieldset">
                  <legend>Protected credentials</legend>
                  <div class="sdsync-secret-editor">
                    <div><strong>Password</strong><span>{{ selectedProfileModel && selectedProfileModel.has_password ? 'Stored · masked' : 'Not stored' }}</span></div>
                    <v-single-select v-model="secretModes.password" :options="secretModeOptions" width="210" :disabled="!canManageSecrets" />
                    <v-input v-if="secretModes.password === 'replace'" v-model="secretValues.password" type="password" maxlength="4096" autocomplete="new-password" placeholder="New password" />
                  </div>
                  <div class="sdsync-secret-editor">
                    <div><strong>TOTP seed</strong><span>{{ selectedProfileModel && selectedProfileModel.has_totp ? 'Stored · masked' : 'Not stored' }}</span></div>
                    <v-single-select v-model="secretModes.totp" :options="secretModeOptions" width="210" :disabled="!canManageSecrets" />
                    <v-input v-if="secretModes.totp === 'replace'" v-model="secretValues.totp" type="password" maxlength="4096" autocomplete="off" placeholder="Base32 seed or otpauth URI" />
                  </div>
                  <p class="sdsync-field-note">Secret values are sent only in the protected request body. They are never returned to this window.</p>
                </fieldset>

                <v-checkbox v-model="profileForm.make_default" :disabled="!canMutate">Use as default profile</v-checkbox>
                <div class="sdsync-form-actions">
                  <v-button v-if="selectedProfile" suffix="red" :disabled="!canMutate || operationBusy" @click="removeProfile">Delete profile</v-button>
                  <span />
                  <v-button suffix="cancel" @click="closeProfile">Cancel</v-button>
                  <v-button suffix="main" html-type="submit" :disabled="!canMutate || operationBusy">Save profile</v-button>
                </div>
              </v-form>
            </div>
          </section>

          <section v-else-if="route === 'routines'" class="sdsync-page" aria-labelledby="sdsync-routines-title">
            <div class="sdsync-section-heading"><div><p class="sdsync-eyebrow">Automation</p><h2 id="sdsync-routines-title">Routines</h2><p>Give each profile an interval, daily-window, or realtime policy with bounded retries, dependencies, and no overlapping syncs.</p></div></div>
            <div class="sdsync-two-column sdsync-routines-grid">
              <v-form v-model="routineForm" class="sdsync-panel" direction="vertical" @submit="saveRoutine">
                <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Package controller</p><h3>Profile routine</h3></div><span :class="pillClass(selectedRoutine ? selectedRoutine.state : 'unknown')">{{ selectedRoutine ? (selectedRoutine.state || (selectedRoutine.enabled ? 'Enabled' : 'Disabled')) : 'New' }}</span></div>
                <v-form-item label="Profile"><v-single-select v-model="routineForm.profile" :options="profileOptions" width="100%" :disabled="!canMutate || operationBusy" @input="loadRoutine" /></v-form-item>
                <v-checkbox v-model="routineForm.enabled" :disabled="!canMutate">Enable routine</v-checkbox>
                <div class="sdsync-form-grid compact">
                  <v-form-item label="Action"><v-single-select v-model="routineForm.action" :options="routineActionOptions" width="100%" /></v-form-item>
                  <v-form-item label="Mode"><v-single-select v-model="routineForm.mode" :options="routineModeOptions" width="100%" /></v-form-item>
                  <v-form-item label="Interval (seconds)"><v-input v-model="routineForm.interval_seconds" number-only /></v-form-item>
                  <v-form-item label="Window starts"><input v-model="routineForm.time_window_start" class="sdsync-native-input" type="time" aria-label="Window starts"></v-form-item>
                  <v-form-item label="Window ends"><input v-model="routineForm.time_window_end" class="sdsync-native-input" type="time" aria-label="Window ends"></v-form-item>
                  <v-form-item label="Realtime debounce (seconds)"><v-input v-model="routineForm.debounce_seconds" number-only /></v-form-item>
                  <v-form-item label="Fallback poll (seconds)"><v-input v-model="routineForm.poll_seconds" number-only /></v-form-item>
                  <v-form-item label="Retry attempts"><v-input v-model="routineForm.retry_count" number-only /></v-form-item>
                  <v-form-item label="Retry backoff (seconds)"><v-input v-model="routineForm.retry_backoff_seconds" number-only /></v-form-item>
                  <v-form-item class="span-2" label="Wait for routines">
                    <select v-model="routineForm.depends_on" class="sdsync-native-input" multiple size="4" aria-label="Wait for routines">
                      <option v-for="profile in dependencyProfiles" :key="profile.name" :value="profile.name">{{ profile.name }}</option>
                    </select>
                  </v-form-item>
                </div>
                <fieldset class="sdsync-weekday-fieldset"><legend>Active weekdays</legend><div class="sdsync-weekdays"><label v-for="day in weekdayOptions" :key="day.value"><input v-model="routineForm.weekdays" type="checkbox" :value="day.value"><span>{{ day.label }}</span></label></div></fieldset>
                <fieldset class="sdsync-danger-fieldset"><legend>Routine deletion guard</legend><v-checkbox v-model="routineForm.allow_delete" :disabled="!canMutate">Permit profile deletion rules</v-checkbox><v-form-item label="Routine deletion approval ceiling"><v-input v-model="routineForm.max_total_delete" number-only /></v-form-item></fieldset>
                <div class="sdsync-form-actions"><v-button suffix="red" :disabled="!canMutate || !selectedRoutine || operationBusy" @click="removeRoutine">Remove routine</v-button><span /><v-button suffix="main" html-type="submit" :disabled="!canMutate || !routineForm.profile || operationBusy">Save routine</v-button></div>
              </v-form>
              <div class="sdsync-stack">
                <article class="sdsync-panel"><div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Configured routines</p><h3>Per-profile automation</h3></div></div><p v-if="!routines.length" class="sdsync-empty">No configured routines.</p><button v-for="routine in routines" :key="routine.profile" type="button" class="sdsync-routine-row" :disabled="operationBusy" @click="selectRoutine(routine.profile)"><span><strong>{{ routine.profile }}</strong><small>{{ routine.mode || 'interval' }} · {{ routine.backend || 'fallback unreported' }} · {{ routine.state || (routine.enabled ? 'enabled' : 'disabled') }}</small></span><time>{{ routine.enabled ? formatDate(routine.next_run_epoch) : 'Disabled' }}</time></button></article>
                <article class="sdsync-panel"><p class="sdsync-eyebrow">Timing model</p><h3>Bounded and non-overlapping</h3><div class="sdsync-timeline"><span>Observe</span><i /><span>Debounce</span><i /><span>Preflight</span><i /><span>Run</span></div><p>Realtime uses package polling when native change hooks are unavailable. Long-running syncs cannot pile up.</p></article>
                <article class="sdsync-panel"><p class="sdsync-eyebrow">Safety invariant</p><h3>One host-local lock</h3><p>Manual plans, manual syncs, and routine runs share one lock.</p></article>
              </div>
            </div>
          </section>

          <section v-else-if="route === 'health'" class="sdsync-page" aria-labelledby="sdsync-health-title">
            <div class="sdsync-section-heading"><div><p class="sdsync-eyebrow">Diagnostics</p><h2 id="sdsync-health-title">Health / Doctor</h2><p>Prove source readability, routing, APIs, authentication, and exact destination access before a sync.</p></div></div>
            <div class="sdsync-two-column">
              <v-form v-model="doctorForm" class="sdsync-panel" direction="vertical" @submit="runDoctor">
                <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Target doctor</p><h3>Run a diagnostic</h3></div><span class="sdsync-pill neutral">Manual</span></div>
                <v-form-item label="Scope"><v-single-select v-model="doctorForm.scope" :options="scopeOptions" width="100%" :disabled="!canMutate" /></v-form-item>
                <v-checkbox v-model="doctorForm.write_test" :disabled="!canMutate || !hasCapability('write_test')">Disposable write test</v-checkbox>
                <div v-if="doctorForm.write_test" class="sdsync-warning"><strong>This mutates the selected target briefly.</strong><v-checkbox v-model="doctorForm.write_confirm" :disabled="!canMutate">I prepared a non-critical destination and approve probe cleanup.</v-checkbox></div>
                <v-button suffix="main" html-type="submit" :disabled="!canMutate || operationBusy">Run doctor</v-button>
              </v-form>
              <article class="sdsync-panel sdsync-diagnostic" aria-live="polite"><div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Latest diagnostic</p><h3>{{ diagnostic.title }}</h3></div><span class="sdsync-pulse" /></div><pre>{{ diagnostic.output }}</pre></article>
            </div>
            <div class="sdsync-check-grid"><article><span>01</span><div><strong>Source integrity</strong><p>Walks the complete source and hashes every payload file.</p></div></article><article><span>02</span><div><strong>Reverse-proxy routing</strong><p>Requires File Station API discovery rather than an HTML fallback.</p></div></article><article><span>03</span><div><strong>Destination permission</strong><p>Checks the exact destination or its nearest existing ancestor.</p></div></article><article><span>04</span><div><strong>Disposable write path</strong><p>Explicit opt-in only, with cleanup evidence.</p></div></article></div>
            <article class="sdsync-panel"><div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Cached per-profile evidence</p><h3>Target health</h3></div><span class="sdsync-freshness">{{ healthFreshness }}</span></div><div class="sdsync-table-wrap"><table><thead><tr><th>Profile</th><th>Last check</th><th>Reachable</th><th>Auth</th><th>Writable</th><th>Latency</th><th>Last success</th><th>Doctor</th><th>Free space</th></tr></thead><tbody><tr v-if="!healthRows.length"><td colspan="9">No cached target-health evidence.</td></tr><tr v-for="health in healthRows" :key="health.profile"><td>{{ health.profile || 'Unknown' }}</td><td>{{ formatDate(health.last_check_epoch || health.checked_at_epoch || health.checked_epoch) }}</td><td :class="healthClass(health.reachable)">{{ booleanEvidence(health.reachable) }}</td><td :class="healthClass(health.authenticated !== undefined ? health.authenticated : health.auth)">{{ booleanEvidence(health.authenticated !== undefined ? health.authenticated : health.auth) }}</td><td :class="healthClass(health.writable)">{{ booleanEvidence(health.writable) }}</td><td>{{ formatDuration(health.latency_ms) }}</td><td>{{ formatDate(health.last_success_epoch || health.last_successful_sync_epoch) }}</td><td>{{ health.doctor_status || health.last_doctor_status || health.state || 'Unavailable' }}</td><td>{{ health.free_space_proven === true ? formatBytes(health.free_space_bytes) : 'Unavailable' }}</td></tr></tbody></table></div></article>
          </section>

          <section v-else-if="route === 'activity'" class="sdsync-page" aria-labelledby="sdsync-activity-title">
            <div class="sdsync-section-heading"><div><p class="sdsync-eyebrow">Observability</p><h2 id="sdsync-activity-title">Activity / Logs</h2><p>Bounded controller, scheduler, and structured sync output updates while this window is open.</p></div><div class="sdsync-action-row"><v-button suffix="grey" @click="toggleLogs">{{ logsPaused ? 'Resume live updates' : 'Pause live updates' }}</v-button><v-button suffix="grey" @click="clearLogView">Clear view</v-button></div></div>
            <article class="sdsync-panel"><div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Structured activity</p><h3>Recent package events</h3></div><span class="sdsync-freshness">{{ activityEvents.length }} event{{ activityEvents.length === 1 ? '' : 's' }}</span></div><ol class="sdsync-activity-feed"><li v-if="!activityEvents.length" class="sdsync-empty">No recorded package events.</li><li v-for="event in reversedActivity" :key="[event.epoch, event.code, event.profile].join(':')"><time>{{ formatDate(event.epoch) }}</time><strong>{{ event.code || 'unknown.event' }}</strong><small>{{ event.profile || 'none' }} · {{ event.state || 'unknown' }}</small></li></ol></article>
            <article class="sdsync-panel sdsync-log-panel"><div class="sdsync-log-toolbar"><v-single-select v-model="logSource" :options="logSourceOptions" width="180" @input="refreshLogs" /><v-single-select v-model="logLines" :options="logLineOptions" width="150" @input="refreshLogs" /><span>{{ logState }}</span></div><pre tabindex="0">{{ logOutput }}</pre></article>
          </section>

          <section v-else-if="route === 'notifications'" class="sdsync-page" aria-labelledby="sdsync-notifications-title">
            <div class="sdsync-section-heading"><div><p class="sdsync-eyebrow">Attention routing</p><h2 id="sdsync-notifications-title">Notifications</h2><p>Choose package-level DSM desktop alerts and optional signals for this open session.</p></div></div>
            <div class="sdsync-two-column">
              <v-form v-model="alertForm" class="sdsync-panel" direction="vertical" @submit="saveAlerts"><div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">DSM desktop alerts</p><h3>Package alert policy</h3></div><span :class="pillClass(alertForm.enabled ? 'running' : 'disabled')">{{ alertForm.enabled ? 'Enabled' : 'Disabled' }}</span></div><v-checkbox v-model="alertForm.enabled" :disabled="!canMutate">Enable DSM desktop alerts</v-checkbox><v-checkbox v-model="alertForm.on_success" :disabled="!canMutate">Notify on success</v-checkbox><v-checkbox v-model="alertForm.on_failure" :disabled="!canMutate">Notify on failure</v-checkbox><v-form-item label="Failures before alert"><v-input v-model="alertForm.failure_threshold" number-only /></v-form-item><v-form-item label="Cooldown (seconds)"><v-input v-model="alertForm.cooldown_seconds" number-only /></v-form-item><v-button suffix="main" html-type="submit" :disabled="!canMutate || operationBusy">Save DSM alert policy</v-button></v-form>
              <div class="sdsync-stack"><article class="sdsync-panel"><p class="sdsync-eyebrow">Direct desktop delivery</p><h3>Fixed, non-secret messages</h3><ul><li><code>sync_succeeded</code> — fixed completion message</li><li><code>sync_failed</code> — fixed sync failure message</li><li><code>doctor_failed</code> — fixed Doctor failure message</li></ul><p>Details stay in Activity and package logs.</p></article><v-form v-model="notificationForm" class="sdsync-panel" direction="vertical" @submit="saveNotificationPreferences"><div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Open-session signal</p><h3>Browser fallback</h3></div><span :class="pillClass(notificationPermission)">{{ notificationPermission }}</span></div><v-checkbox v-model="notificationForm.desktop_notifications">Notify while this app is open</v-checkbox><v-checkbox v-model="notificationForm.audible">Audible cue</v-checkbox><v-button suffix="grey" html-type="submit">Save session preferences</v-button></v-form></div>
            </div>
          </section>

          <section v-else class="sdsync-page" aria-labelledby="sdsync-settings-title">
            <div class="sdsync-section-heading"><div><p class="sdsync-eyebrow">Application preferences</p><h2 id="sdsync-settings-title">Settings</h2><p>Control this DSM window without changing the sync engine's safety defaults.</p></div></div>
            <div class="sdsync-two-column">
              <v-form v-model="settings" class="sdsync-panel" direction="vertical" @submit="saveInterfaceSettings"><div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Display and refresh</p><h3>Interface</h3></div></div><v-form-item label="Theme"><v-single-select v-model="settings.theme" :options="themeOptions" width="100%" /></v-form-item><v-form-item label="Status refresh"><v-single-select v-model="settings.status_refresh" :options="statusRefreshOptions" width="100%" /></v-form-item><v-form-item label="Log refresh"><v-single-select v-model="settings.log_refresh" :options="logRefreshOptions" width="100%" /></v-form-item><div class="sdsync-form-actions sdsync-settings-actions"><span /><span /><v-button suffix="main" html-type="submit">Save interface settings</v-button></div></v-form>
              <div class="sdsync-stack"><article class="sdsync-panel"><p class="sdsync-eyebrow">Security posture</p><h3>Fail closed by design</h3><dl class="sdsync-definition-grid"><div><dt>DSM access</dt><dd>Administrators only</dd></div><div><dt>Secrets returned</dt><dd>Never</dd></div><div><dt>Mutation method</dt><dd>Authenticated POST</dd></div><div><dt>Schedule default</dt><dd>Disabled</dd></div></dl></article><article class="sdsync-panel"><p class="sdsync-eyebrow">Package paths</p><h3>Runtime ownership</h3><p>Configuration, credentials, state, and logs remain in package-private DSM FHS directories. This window does not make those paths browser-readable.</p></article></div>
            </div>
          </section>

        </main>

        <div class="sdsync-toasts" aria-live="polite" aria-relevant="additions"><div v-for="toastItem in toasts" :key="toastItem.id" :class="['sdsync-toast', { 'is-error': toastItem.error }]" :role="toastItem.error ? 'alert' : 'status'"><strong>{{ toastItem.title }}</strong><span>{{ toastItem.message }}</span></div></div>
        <div v-if="confirmation.visible" class="sdsync-modal-backdrop" role="presentation" @click.self="settleConfirmation(false)">
          <div ref="confirmationDialog" class="sdsync-modal" role="dialog" aria-modal="true" aria-labelledby="sdsync-confirm-title" aria-describedby="sdsync-confirm-message" tabindex="-1">
            <p class="sdsync-eyebrow">Confirm action</p>
            <h2 id="sdsync-confirm-title">{{ confirmation.title }}</h2>
            <p id="sdsync-confirm-message">{{ confirmation.message }}</p>
            <div class="sdsync-action-row">
              <v-button ref="confirmationCancel" suffix="cancel" aria-label="Cancel confirmation" @click="settleConfirmation(false)">Cancel</v-button>
              <v-button ref="confirmationAccept" suffix="red" aria-label="Confirm action" @click="settleConfirmation(true)">{{ confirmation.button }}</v-button>
            </div>
          </div>
        </div>
      </div>
    </v-app-window>
  </v-app-instance>
</template>

<script>
import {
  ACTIONS,
  MAX_RESPONSE_BYTES,
  SNAPSHOT_SCHEMA,
  apiGet,
  apiPost,
  arrayOf,
  boundedText,
  formatBytes,
  formatDate,
  formatDuration,
  numberOr,
  pick
} from "./api";

const SETTINGS_KEY = "sdsync.ui.settings.v1";

function defaults() {
  return { theme: "dark", status_refresh: 5000, log_refresh: 5000, desktop_notifications: false, audible: false };
}

function loadSettings() {
  const fallback = defaults();
  try {
    const parsed = JSON.parse(window.localStorage.getItem(SETTINGS_KEY) || "null");
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return fallback;
    return {
      theme: ["dark", "light", "system"].includes(parsed.theme) ? parsed.theme : fallback.theme,
      status_refresh: [3000, 5000, 10000, 30000].includes(Number(parsed.status_refresh)) ? Number(parsed.status_refresh) : fallback.status_refresh,
      log_refresh: [5000, 10000, 30000].includes(Number(parsed.log_refresh)) ? Number(parsed.log_refresh) : fallback.log_refresh,
      desktop_notifications: parsed.desktop_notifications === true,
      audible: parsed.audible === true
    };
  } catch (_error) {
    return fallback;
  }
}

function emptyProfile() {
  return {
    name: "", source: "", url: "", username: "", remote: "", compare: "content", jobs: 2,
    allow_http: false, delete: false, max_delete: 5, make_default: false, excludes: "",
    allow_empty_source: false, retries: 2, timeout: 7200, connect_timeout: 15, max_rate: 0,
    ca_certificate: "", danger_invalid_certs: false, danger_invalid_confirm: false,
    verbosity: 0, quiet: false, log_level: "info", remote_log_url: "", remote_log_mode: "best-effort"
  };
}

function emptyRoutine(profile = "") {
  return { profile, enabled: false, action: "sync", mode: "interval", interval_seconds: 3600, weekdays: [1, 2, 3, 4, 5, 6, 7], time_window_start: "00:00", time_window_end: "23:59", debounce_seconds: 30, poll_seconds: 30, retry_count: 2, retry_backoff_seconds: 60, allow_delete: false, max_total_delete: 100, depends_on: [] };
}

function options(entries) {
  return entries.map(([value, label]) => ({ value, label }));
}

export default {
  name: "SynologyDriveSyncApp",
  data() {
    const settings = loadSettings();
    return {
      routes: [
        { id: "overview", title: "Overview", icon: "⌁" }, { id: "profiles", title: "Profiles", icon: "▤" },
        { id: "routines", title: "Routines", icon: "◷" }, { id: "health", title: "Health / Doctor", icon: "◇" },
        { id: "activity", title: "Activity / Logs", icon: "≋" }, { id: "notifications", title: "Notifications", icon: "◉" },
        { id: "settings", title: "Settings", icon: "⚙" }
      ],
      route: "overview", auth: { signal: undefined }, csrfToken: "", snapshot: null,
      connected: false, connectionLabel: "Connecting to package…", freshness: "Waiting for status",
      snapshotTimer: 0, logTimer: 0, snapshotLoading: false, logsLoading: false, operationBusy: false,
      settings, profileFilter: "", profileEditorOpen: false, selectedProfile: "", profileForm: emptyProfile(),
      secretModes: { password: "keep", totp: "keep", remote_log_token: "keep" },
      secretValues: { password: "", totp: "", remote_log_token: "" },
      routineForm: emptyRoutine(), doctorForm: { scope: "all", write_test: false, write_confirm: false },
      alertForm: { enabled: false, on_success: false, on_failure: true, failure_threshold: 1, cooldown_seconds: 3600 },
      notificationForm: { desktop_notifications: settings.desktop_notifications, audible: settings.audible },
      diagnostic: { title: "Not run in this session", output: "No diagnostic output yet." },
      logsPaused: false, logSource: "all", logLines: 200, logState: "Waiting for logs", logOutput: "No log data yet.", activityEvents: [],
      lastFailureKey: "", toasts: [], toastSequence: 0,
      confirmation: { visible: false, title: "", message: "", button: "Confirm", resolve: null },
      confirmationPriorFocus: null, confirmationKeyHandler: null,
      systemLight: false, visibilityHandler: null, mediaQuery: null, mediaHandler: null,
      toastTimers: [], abortController: null, disposed: false
    };
  },
  computed: {
    pageTitle() { const found = this.routes.find((item) => item.id === this.route); return found ? found.title : "Overview"; },
    profiles() { return arrayOf(this.snapshot && this.snapshot.profiles); },
    routines() { return arrayOf(this.snapshot && this.snapshot.routines); },
    filteredProfiles() { const query = this.profileFilter.trim().toLowerCase(); return query ? this.profiles.filter((profile) => String(profile.name || "").toLowerCase().includes(query)) : this.profiles; },
    enabledRoutines() { return this.routines.filter((routine) => routine.enabled === true); },
    realtimeRoutines() { return this.enabledRoutines.filter((routine) => routine.mode === "realtime"); },
    realtimeDetail() { const fallbacks = this.realtimeRoutines.filter((routine) => String(routine.backend || "").includes("poll")).length; return !this.realtimeRoutines.length ? "No enabled realtime routine" : (fallbacks ? `${fallbacks} using polling fallback` : "Native/fallback backend reported healthy"); },
    readyProfileCount() { return this.profiles.filter((profile) => profile.has_password === true).length; },
    capabilities() { return this.snapshot && this.snapshot.capabilities && typeof this.snapshot.capabilities === "object" ? this.snapshot.capabilities : {}; },
    canMutate() { return this.capabilities.mutations === true && Boolean(this.csrfToken); },
    canManageSecrets() { return this.canMutate && this.capabilities.secrets === true; },
    selectedProfileModel() { return this.profiles.find((profile) => String(profile.name) === String(this.selectedProfile)) || null; },
    selectedRoutine() { return this.routines.find((routine) => String(routine.profile) === String(this.routineForm.profile)) || null; },
    dependencyProfiles() { return this.profiles.filter((profile) => String(profile.name) !== String(this.routineForm.profile)); },
    profileOptions() { return options([["", "Choose a profile"], ...this.profiles.map((profile) => [String(profile.name), String(profile.name)])]); },
    scopeOptions() { return options([["all", "All profiles"], ...this.profiles.map((profile) => [String(profile.name), String(profile.name)])]); },
    run() { return this.snapshot && this.snapshot.run && typeof this.snapshot.run === "object" ? this.snapshot.run : ((this.snapshot && this.snapshot.last_run) || {}); },
    runStatus() { return boundedText(pick(this.run, "status", "state", "result"), "Unavailable"); },
    runScope() { return boundedText(this.run.scope, "Unavailable"); },
    runOperation() { return boundedText(this.run.operation, "Unavailable"); },
    lastRunDetail() { return this.run.finished_epoch ? formatDate(this.run.finished_epoch) : "No completion time"; },
    serviceState() { const service = this.snapshot && this.snapshot.service; const value = service && typeof service === "object" ? pick(service, "state", "status") : service; return boundedText(value, this.snapshot ? "unknown" : "Unavailable"); },
    overviewSummary() { return this.serviceState === "running" ? "The package controller is running. Status and logs update while this window remains open." : `The package controller reports ${this.serviceState}. Review Health and Activity before relying on automation.`; },
    nextRun() { const epochs = this.enabledRoutines.map((routine) => Number(routine.next_run_epoch)).filter((value) => Number.isFinite(value) && value > 0); return epochs.length ? formatDate(Math.min(...epochs)) : "None"; },
    healthRows() { const explicit = this.snapshot && this.snapshot.health; if (Array.isArray(explicit)) return explicit; return this.profiles.map((profile) => { const health = profile.health && typeof profile.health === "object" ? profile.health : {}; const routine = this.routines.find((item) => String(item.profile) === String(profile.name)) || {}; return Object.assign({ profile: profile.name, last_success_epoch: routine.last_success_epoch }, health); }); },
    healthFreshness() { const newest = this.healthRows.reduce((value, health) => Math.max(value, numberOr(health.last_check_epoch || health.checked_at_epoch || health.checked_epoch, 0)), 0); return newest ? `Newest check ${formatDate(newest)}` : "Cached time unavailable"; },
    reversedActivity() { return this.activityEvents.slice().reverse(); },
    notificationPermission() { return window.Notification ? Notification.permission : "unsupported"; },
    themeClass() { const theme = this.settings.theme === "system" ? (this.systemLight ? "is-light" : "is-dark") : `is-${this.settings.theme}`; return theme; },
    compareOptions() { return options([["content", "Content — size, MD5, mtime"], ["metadata", "Metadata — size and mtime"], ["size-only", "Size only"]]); },
    verbosityOptions() { return options([[0, "Normal"], [1, "Verbose"], [2, "Very verbose"]]); },
    logLevelOptions() { return options(["trace", "debug", "info", "warn", "error", "off"].map((value) => [value, value])); },
    remoteLogModeOptions() { return options([["best-effort", "Best effort"], ["required", "Required"]]); },
    secretModeOptions() { return options([["keep", "Keep existing"], ["replace", "Replace securely"], ["clear", "Clear stored value"]]); },
    routineActionOptions() { return options([["sync", "Sync"], ["plan", "Plan only"]]); },
    routineModeOptions() { return options([["interval", "Interval"], ["daily", "Daily window"], ["realtime", "Realtime watcher"]]); },
    weekdayOptions() { return options([[1, "Mon"], [2, "Tue"], [3, "Wed"], [4, "Thu"], [5, "Fri"], [6, "Sat"], [7, "Sun"]]); },
    logSourceOptions() { return options([["all", "All logs"], ["controller", "Controller"], ["scheduler", "Scheduler"], ["sync", "Sync"]]); },
    logLineOptions() { return options([[100, "100 lines"], [200, "200 lines"], [500, "500 lines"], [1000, "1000 lines"]]); },
    themeOptions() { return options([["dark", "Dark"], ["system", "Follow system"], ["light", "Light"]]); },
    statusRefreshOptions() { return options([[3000, "Every 3 seconds"], [5000, "Every 5 seconds"], [10000, "Every 10 seconds"], [30000, "Every 30 seconds"]]); },
    logRefreshOptions() { return options([[5000, "Every 5 seconds"], [10000, "Every 10 seconds"], [30000, "Every 30 seconds"]]); }
  },
  async mounted() {
    this.abortController = typeof window.AbortController === "function" ? new window.AbortController() : null;
    this.auth = { signal: this.abortController ? this.abortController.signal : undefined };
    this.mediaQuery = window.matchMedia ? window.matchMedia("(prefers-color-scheme: light)") : null;
    this.systemLight = Boolean(this.mediaQuery && this.mediaQuery.matches);
    this.mediaHandler = (event) => { this.systemLight = event.matches; };
    if (this.mediaQuery && this.mediaQuery.addEventListener) this.mediaQuery.addEventListener("change", this.mediaHandler);
    this.visibilityHandler = () => {
      if (document.hidden) {
        this.stopTimers();
        this.clearSecrets();
      } else {
        this.refreshSnapshot(false);
        if (this.route === "activity") this.refreshLogs();
      }
    };
    document.addEventListener("visibilitychange", this.visibilityHandler);
    try {
      await this.refreshCsrf();
    } catch (error) {
      if (this.disposed) return;
      this.connected = false;
      this.connectionLabel = "DSM session authentication unavailable";
      this.toast("Control bridge unavailable", boundedText(error.message, "Sign in to DSM again, then reopen this app."), true);
    }
    if (this.disposed) return;
    await this.refreshSnapshot(false);
  },
  beforeDestroy() {
    this.disposed = true;
    if (this.abortController) this.abortController.abort();
    this.stopTimers();
    this.toastTimers.forEach((timer) => window.clearTimeout(timer));
    this.toastTimers = [];
    if (this.visibilityHandler) document.removeEventListener("visibilitychange", this.visibilityHandler);
    if (this.mediaQuery && this.mediaQuery.removeEventListener && this.mediaHandler) this.mediaQuery.removeEventListener("change", this.mediaHandler);
    this.removeConfirmationKeyHandler();
    if (this.confirmation.resolve) this.confirmation.resolve(false);
    this.confirmationPriorFocus = null;
    this.clearSecrets();
    this.csrfToken = "";
    this.auth = { signal: undefined };
  },
  methods: {
    formatBytes, formatDate, formatDuration,
    navigate(route) { if (!this.routes.some((item) => item.id === route)) return; if (this.route === "profiles" && route !== "profiles") this.closeProfile(); this.route = route; if (route === "activity") this.refreshLogs(); else window.clearTimeout(this.logTimer); },
    pillClass(state) { const value = String(state || "unknown").toLowerCase(); return ["sdsync-pill", { failed: ["failed", "error", "untrusted", "denied"].includes(value), neutral: ["disabled", "stopped", "unknown", "default", "unsupported", "unavailable"].includes(value) }]; },
    healthClass(value) { return value === true ? "sdsync-health-ok" : (value === false ? "sdsync-health-bad" : "sdsync-health-unknown"); },
    booleanEvidence(value) { return value === true ? "Yes" : (value === false ? "No" : "Unavailable"); },
    reportMutationError(error, failedTitle, unknownTitle, fallback) {
      const unknown = Boolean(error && error.outcomeUnknown === true);
      const observed = boundedText(error && error.message, fallback);
      const message = unknown
        ? boundedText(
          `${observed} The request was already queued; do not retry it or create a duplicate. Inspect Activity and Logs for the eventual outcome.`,
          "The queued operation outcome is unknown. Do not retry it; inspect Activity and Logs."
        )
        : observed;
      this.toast(unknown ? unknownTitle : failedTitle, message, !unknown);
      return { unknown, message };
    },
    hasCapability(name) { return this.capabilities[name] === true; },
    integer(value, fallback) { const parsed = Number(value); return Number.isInteger(parsed) ? parsed : fallback; },
    between(value, minimum, maximum) { const parsed = Number(value); return Number.isInteger(parsed) && parsed >= minimum && parsed <= maximum; },
    toast(title, message, error = false) { if (this.disposed) return; const item = { id: ++this.toastSequence, title, message, error }; this.toasts.push(item); const timer = window.setTimeout(() => { if (this.disposed) return; const index = this.toasts.findIndex((candidate) => candidate.id === item.id); if (index >= 0) this.toasts.splice(index, 1); this.toastTimers = this.toastTimers.filter((candidate) => candidate !== timer); }, 6000); this.toastTimers.push(timer); },
    stopTimers() { window.clearTimeout(this.snapshotTimer); window.clearTimeout(this.logTimer); this.snapshotTimer = 0; this.logTimer = 0; },
    scheduleSnapshot() { window.clearTimeout(this.snapshotTimer); if (!this.disposed && !document.hidden) this.snapshotTimer = window.setTimeout(() => this.refreshSnapshot(false), Number(this.settings.status_refresh)); },
    scheduleLogs() { window.clearTimeout(this.logTimer); if (!this.disposed && !document.hidden && this.route === "activity" && !this.logsPaused) this.logTimer = window.setTimeout(() => this.refreshLogs(), Number(this.settings.log_refresh)); },
    async refreshCsrf() { if (this.disposed) return; this.csrfToken = ""; const model = await apiGet(this.auth, "csrf"); if (this.disposed) return; if (typeof model.csrf_token !== "string" || !model.csrf_token || model.csrf_token.length > 4096) throw new Error("Authenticated bridge did not issue a valid CSRF token"); this.csrfToken = model.csrf_token; },
    async refreshSnapshot(manual) {
      if (this.disposed || this.snapshotLoading || document.hidden) return;
      this.snapshotLoading = true;
      try {
        if (!this.csrfToken) await this.refreshCsrf();
        if (this.disposed) return;
        const snapshot = await apiGet(this.auth, "snapshot");
        if (this.disposed) return;
        if (snapshot.schema !== SNAPSHOT_SCHEMA) throw new Error("Unsupported DSM API schema");
        this.snapshot = snapshot;
        if (typeof snapshot.csrf_token === "string" && snapshot.csrf_token) this.csrfToken = snapshot.csrf_token;
        this.connected = true;
        this.connectionLabel = this.canMutate ? "Authenticated control bridge" : "Package status · read-only";
        this.freshness = `Updated ${new Intl.DateTimeFormat(undefined, { timeStyle: "medium" }).format(new Date())}`;
        this.hydrateAlerts();
        this.maybeNotifyFailure();
        if (manual) this.toast("Status refreshed", "The latest package snapshot is displayed.");
      } catch (error) {
        if (this.disposed) return;
        this.snapshot = null; this.csrfToken = ""; this.connected = false; this.connectionLabel = "Control bridge unavailable"; this.freshness = "Status unavailable";
        if (manual) this.toast("Refresh failed", boundedText(error.message, "Unable to read package status."), true);
      } finally { this.snapshotLoading = false; if (!this.disposed) this.scheduleSnapshot(); }
    },
    hydrateAlerts() { const alerts = this.snapshot && this.snapshot.alerts; if (!alerts || typeof alerts !== "object") return; this.alertForm = { enabled: alerts.enabled === true, on_success: alerts.on_success === true, on_failure: alerts.on_failure !== false, failure_threshold: numberOr(alerts.failure_threshold, 1), cooldown_seconds: numberOr(alerts.cooldown_seconds, 3600) }; },
    openProfile(name) {
      if (this.operationBusy) return;
      const profile = name ? this.profiles.find((item) => String(item.name) === String(name)) : null;
      this.selectedProfile = profile ? String(profile.name) : "";
      this.profileForm = emptyProfile();
      if (profile) this.profileForm = Object.assign(this.profileForm, { name: pick(profile, "name") || "", source: pick(profile, "source") || "", url: pick(profile, "url") || "", username: pick(profile, "username") || "", remote: pick(profile, "remote", "remote_path") || "", compare: pick(profile, "compare") || "content", jobs: numberOr(pick(profile, "jobs"), 2), allow_http: pick(profile, "allow_http") === true, delete: pick(profile, "delete") === true, max_delete: numberOr(pick(profile, "max_delete"), 5), make_default: pick(profile, "is_default", "default") === true, excludes: arrayOf(profile.excludes).join("\n"), allow_empty_source: pick(profile, "allow_empty_source") === true, retries: numberOr(pick(profile, "retries"), 2), timeout: numberOr(pick(profile, "timeout", "upload_timeout_seconds"), 7200), connect_timeout: numberOr(pick(profile, "connect_timeout", "connect_timeout_seconds"), 15), max_rate: numberOr(pick(profile, "max_rate", "max_rate_bytes_per_second"), 0), ca_certificate: pick(profile, "ca_certificate") || "", danger_invalid_certs: pick(profile, "danger_invalid_certs", "danger_accept_invalid_certs") === true, verbosity: numberOr(pick(profile, "verbosity"), 0), quiet: pick(profile, "quiet") === true, log_level: pick(profile, "log_level") || "info", remote_log_url: pick(profile, "remote_log_url") || "", remote_log_mode: pick(profile, "remote_log_mode") || "best-effort" });
      this.secretModes = { password: "keep", totp: "keep", remote_log_token: "keep" }; this.clearSecrets(); this.profileEditorOpen = true;
    },
    closeProfile() { this.clearSecrets(); this.secretModes = { password: "keep", totp: "keep", remote_log_token: "keep" }; this.profileEditorOpen = false; this.selectedProfile = ""; },
    clearSecrets() { this.secretValues = { password: "", totp: "", remote_log_token: "" }; },
    profilePayload() { const maxRate = this.integer(this.profileForm.max_rate, 0); return { name: this.profileForm.name, source: this.profileForm.source, url: this.profileForm.url, username: this.profileForm.username, remote: this.profileForm.remote, compare: this.profileForm.compare, jobs: this.integer(this.profileForm.jobs, 2), allow_http: this.profileForm.allow_http === true, delete: this.profileForm.delete === true, max_delete: this.integer(this.profileForm.max_delete, 5), make_default: this.profileForm.make_default === true, excludes: String(this.profileForm.excludes || "").split(/\r?\n/).map((item) => item.trim()).filter(Boolean), allow_empty_source: this.profileForm.allow_empty_source === true, retries: this.integer(this.profileForm.retries, 2), timeout_seconds: this.integer(this.profileForm.timeout, 7200), connect_timeout_seconds: this.integer(this.profileForm.connect_timeout, 15), max_rate_bytes_per_second: maxRate === 0 ? null : maxRate, ca_certificate: this.profileForm.ca_certificate || null, danger_accept_invalid_certs: this.profileForm.danger_invalid_certs === true, verbosity: this.integer(this.profileForm.verbosity, 0), quiet: this.profileForm.quiet === true, log_level: this.profileForm.log_level, remote_log_url: this.profileForm.remote_log_url || null, remote_log_mode: this.profileForm.remote_log_mode }; },
    secretOperations(profile) { return [["password", "password"], ["totp", "totp"], ["remote_log_token", "remote-log-token"]].filter(([field]) => this.secretModes[field] !== "keep").map(([field, kind]) => ({ profile, kind, mode: this.secretModes[field], value: this.secretModes[field] === "replace" ? this.secretValues[field] : null })); },
    validateProfile(payload, secrets) {
      if (!/^[A-Za-z0-9_-]{1,64}$/.test(payload.name)) return "Name must use letters, digits, underscore, or hyphen.";
      if (!payload.source || !payload.url || !payload.username || !payload.remote) return "Name, source, URL, username, and remote path are required.";
      if (payload.quiet && payload.verbosity !== 0) return "Quiet output cannot be combined with verbose output.";
      if (payload.danger_accept_invalid_certs && !this.profileForm.danger_invalid_confirm) return "Explicitly accept the TLS interception risk.";
      if (payload.remote_log_url && !payload.remote_log_url.startsWith("https://")) return "Remote log delivery requires an HTTPS URL.";
      if (secrets.some((item) => item.mode === "replace" && !item.value)) return "Replacement secret values cannot be empty.";
      if (!this.between(payload.jobs, 1, 16)) return "Concurrent uploads must be between 1 and 16.";
      if (!this.between(payload.max_delete, 0, 2147483647)) return "Maximum deletions must be a non-negative integer.";
      if (!this.between(payload.retries, 0, 5)) return "Retries must be between 0 and 5.";
      if (!this.between(payload.timeout_seconds, 1, 86400)) return "Upload timeout must be between 1 and 86400 seconds.";
      if (!this.between(payload.connect_timeout_seconds, 1, 600)) return "Connect timeout must be between 1 and 600 seconds.";
      if (payload.max_rate_bytes_per_second !== null && !this.between(payload.max_rate_bytes_per_second, 1, Number.MAX_SAFE_INTEGER)) return "Maximum rate must be zero or a positive integer.";
      if (!this.between(payload.verbosity, 0, 2)) return "Verbosity must be Normal, Verbose, or Very verbose.";
      return "";
    },
    async saveProfile(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (!this.canMutate || this.operationBusy) return;
      const payload = this.profilePayload(); const secrets = this.secretOperations(payload.name); const error = this.validateProfile(payload, secrets);
      if (error) return this.toast("Profile not saved", error, true);
      const risky = payload.allow_empty_source || payload.danger_accept_invalid_certs || payload.delete;
      if (risky && !await this.confirmAction("Save dangerous profile settings?", "Review deletion, empty-source, and TLS settings before continuing.", "Save profile")) return;
      this.operationBusy = true; this.clearSecrets();
      let configurationApplied = false;
      let secretsApplied = 0;
      try {
        await apiPost(this.auth, this.csrfToken, ACTIONS.configureProfile, payload);
        configurationApplied = true;
        if (this.disposed) return;
        for (const secret of secrets) {
          await apiPost(this.auth, this.csrfToken, ACTIONS.setSecret, secret);
          secretsApplied += 1;
          if (this.disposed) return;
        }
        this.toast("Profile saved", "The controller applied the validated configuration and protected credential operations.");
        this.closeProfile();
        await this.refreshSnapshot(false);
      } catch (caught) {
        if (this.disposed) return;
        const partiallyApplied = configurationApplied || secretsApplied > 0;
        const reportedError = partiallyApplied && caught.outcomeUnknown !== true
          ? new Error(`${boundedText(caught.message, "A later profile stage failed.")} Earlier profile stages were applied; inspect credential presence before retrying.`)
          : caught;
        this.reportMutationError(
          reportedError,
          partiallyApplied ? "Profile partially applied" : "Profile not saved",
          partiallyApplied ? "Profile partially applied · outcome unknown" : "Profile outcome unknown",
          "The package rejected the change."
        );
        if (partiallyApplied || caught.outcomeUnknown === true) {
          this.closeProfile();
          await this.refreshSnapshot(false);
        }
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    async removeProfile() {
      if (!this.canMutate || !this.selectedProfile || this.operationBusy) return;
      const name = this.selectedProfile;
      if (!await this.confirmAction(`Delete profile ${name}?`, "This removes package-owned configuration and protected credentials. Synced files are not deleted.", "Delete profile")) return;
      this.operationBusy = true;
      try {
        await apiPost(this.auth, this.csrfToken, ACTIONS.removeProfile, { name });
        if (this.disposed) return;
        this.toast("Profile deleted", `The controller removed ${name} and its stored credentials.`);
        this.closeProfile();
        await this.refreshSnapshot(false);
      } catch (error) {
        if (this.disposed) return;
        this.reportMutationError(error, "Profile not deleted", "Profile deletion outcome unknown", "The package rejected the change.");
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    loadRoutine(profileName) { const profile = typeof profileName === "string" ? profileName : this.routineForm.profile; const routine = this.routines.find((item) => String(item.profile) === String(profile)); this.routineForm = routine ? { profile, enabled: routine.enabled === true, action: routine.action || "sync", mode: routine.mode || "interval", interval_seconds: numberOr(routine.interval_seconds, 3600), weekdays: Array.isArray(routine.weekdays) ? routine.weekdays.map(Number) : String(routine.weekdays || "1,2,3,4,5,6,7").split(",").map(Number), time_window_start: routine.time_window_start || routine.window_start || "00:00", time_window_end: routine.time_window_end || routine.window_end || "23:59", debounce_seconds: numberOr(routine.debounce_seconds, 30), poll_seconds: numberOr(routine.poll_seconds, 30), retry_count: numberOr(routine.retry_count, 2), retry_backoff_seconds: numberOr(routine.retry_backoff_seconds, 60), allow_delete: routine.allow_delete === true, max_total_delete: numberOr(routine.max_total_delete, 100), depends_on: arrayOf(routine.depends_on).map(String) } : emptyRoutine(profile); },
    selectRoutine(profile) { if (this.operationBusy) return; this.routineForm.profile = profile; this.loadRoutine(profile); },
    routinePayload() { return { profile: this.routineForm.profile, enabled: this.routineForm.enabled === true, action: this.routineForm.action, mode: this.routineForm.mode, interval_seconds: this.integer(this.routineForm.interval_seconds, 3600), weekdays: this.routineForm.weekdays.map(Number), time_window_start: this.routineForm.time_window_start, time_window_end: this.routineForm.time_window_end, debounce_seconds: this.integer(this.routineForm.debounce_seconds, 30), poll_seconds: this.integer(this.routineForm.poll_seconds, 30), retry_count: this.integer(this.routineForm.retry_count, 2), retry_backoff_seconds: this.integer(this.routineForm.retry_backoff_seconds, 60), allow_delete: this.routineForm.allow_delete === true, max_total_delete: this.integer(this.routineForm.max_total_delete, 100), depends_on: this.routineForm.depends_on.map(String) }; },
    async saveRoutine(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (!this.canMutate || !this.routineForm.profile || this.operationBusy) return;
      const payload = this.routinePayload();
      if (!payload.weekdays.length) return this.toast("Routine not saved", "Select at least one active weekday.", true);
      if (!this.between(payload.interval_seconds, 60, 2592000) || !this.between(payload.debounce_seconds, 5, 3600) || !this.between(payload.poll_seconds, 5, 3600) || !this.between(payload.retry_count, 0, 5) || !this.between(payload.retry_backoff_seconds, 10, 86400) || !this.between(payload.max_total_delete, 0, 2147483647)) return this.toast("Routine not saved", "One or more timing, retry, or deletion limits are outside the supported range.", true);
      if (!/^([01]\d|2[0-3]):[0-5]\d$/.test(payload.time_window_start) || !/^([01]\d|2[0-3]):[0-5]\d$/.test(payload.time_window_end)) return this.toast("Routine not saved", "Daily window times must use 24-hour HH:MM format.", true);
      this.operationBusy = true;
      try {
        await apiPost(this.auth, this.csrfToken, ACTIONS.routine, payload);
        if (this.disposed) return;
        this.toast("Routine saved", "The controller applied the per-profile policy.");
        await this.refreshSnapshot(false);
        if (!this.disposed) this.loadRoutine(payload.profile);
      } catch (error) {
        if (this.disposed) return;
        this.reportMutationError(error, "Routine not saved", "Routine outcome unknown", "The package rejected the routine.");
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    async removeRoutine() {
      const profile = this.routineForm.profile;
      if (!this.canMutate || !profile || !this.selectedRoutine || this.operationBusy) return;
      if (!await this.confirmAction(`Remove routine for ${profile}?`, "The profile remains configured, but package automation for it will stop.", "Remove routine")) return;
      this.operationBusy = true;
      try {
        await apiPost(this.auth, this.csrfToken, ACTIONS.removeRoutine, { name: profile });
        if (this.disposed) return;
        this.toast("Routine removed", `The controller removed automation for ${profile}.`);
        await this.refreshSnapshot(false);
        if (!this.disposed) this.loadRoutine(profile);
      } catch (error) {
        if (this.disposed) return;
        this.reportMutationError(error, "Routine not removed", "Routine removal outcome unknown", "The package rejected the change.");
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    async saveAlerts(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (!this.canMutate || this.operationBusy) return;
      const payload = { enabled: this.alertForm.enabled === true, on_success: this.alertForm.on_success === true, on_failure: this.alertForm.on_failure === true, failure_threshold: this.integer(this.alertForm.failure_threshold, 1), cooldown_seconds: this.integer(this.alertForm.cooldown_seconds, 3600) };
      if (!this.between(payload.failure_threshold, 1, 100) || !this.between(payload.cooldown_seconds, 60, 604800)) return this.toast("Alert policy not saved", "Failure threshold or cooldown is outside the supported range.", true);
      this.operationBusy = true;
      try {
        await apiPost(this.auth, this.csrfToken, ACTIONS.alertPolicy, payload);
        if (this.disposed) return;
        this.toast("Alert policy saved", "The controller applied the DSM desktop alert policy.");
        await this.refreshSnapshot(false);
      } catch (error) {
        if (this.disposed) return;
        this.reportMutationError(error, "Alert policy not saved", "Alert policy outcome unknown", "The package rejected the policy.");
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    async executeOperation(kind, payload) {
      if (!this.canMutate || this.operationBusy || this.disposed) return;
      this.operationBusy = true;
      const awaitTerminal = kind === "doctor";
      try {
        const result = await apiPost(
          this.auth,
          this.csrfToken,
          ACTIONS.execute,
          Object.assign({ kind }, payload),
          awaitTerminal
        );
        if (this.disposed) return;
        const message = boundedText(
          result.output || result.message,
          awaitTerminal
            ? "Doctor completed without additional output."
            : "Queued safely; follow Activity and Logs for the final result."
        );
        if (awaitTerminal) {
          this.diagnostic = { title: "Doctor completed", output: message };
        }
        const operation = `${kind.charAt(0).toUpperCase()}${kind.slice(1)}`;
        this.toast(`${operation} ${awaitTerminal ? "completed" : "queued"}`, message);
        await this.refreshSnapshot(false);
      } catch (error) {
        if (this.disposed) return;
        const operation = `${kind.charAt(0).toUpperCase()}${kind.slice(1)}`;
        const report = this.reportMutationError(
          error,
          `${operation} failed`,
          `${operation} outcome unknown`,
          "The package rejected the operation."
        );
        if (kind === "doctor") {
          this.diagnostic = {
            title: report.unknown ? "Doctor outcome unknown" : "Doctor failed",
            output: boundedText(error.resultOutput || report.message, "Diagnostic failed.")
          };
        }
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    quickPlan() { return this.executeOperation("plan", { scope: "all", write_test: null, allow_delete: false, max_total_delete: 0 }); },
    async quickRun() { if (!this.canMutate || this.operationBusy) return; if (await this.confirmAction("Run all configured profiles?", "This starts a real one-way sync. Remote deletion stays disabled for this quick action.", "Run all")) return this.executeOperation("run", { scope: "all", write_test: null, allow_delete: false, max_total_delete: 0 }); },
    async runDoctor(event) { if (event && event.preventDefault) event.preventDefault(); if (!this.canMutate || this.operationBusy) return; if (this.doctorForm.write_test && !this.doctorForm.write_confirm) return this.toast("Write-test confirmation required", "Approve the disposable probe and cleanup before running.", true); if (this.doctorForm.write_test && !await this.confirmAction("Run the disposable target probe?", "The doctor briefly creates, verifies, and removes a unique probe in the selected destination.", "Run write test")) return; this.diagnostic = { title: "Doctor running", output: "Waiting for the package controller…" }; return this.executeOperation("doctor", { scope: this.doctorForm.scope, write_test: this.doctorForm.write_test, allow_delete: null, max_total_delete: null }); },
    logsFrom(model) { if (Array.isArray(model.logs)) return model.logs.map((entry) => { if (typeof entry === "string") return entry; if (entry && typeof entry === "object") { if (Array.isArray(entry.lines)) return entry.lines.map((line) => `[${boundedText(entry.source, "log")}] ${boundedText(line, "")}`).join("\n"); return `${entry.timestamp ? `[${entry.timestamp}] ` : ""}${entry.source ? `[${entry.source}] ` : ""}${boundedText(entry.message, "")}`; } return ""; }).join("\n"); return boundedText(model.text || model.output, "No log data yet."); },
    async refreshLogs() { if (this.disposed || this.logsLoading || this.logsPaused || document.hidden || this.route !== "activity") return; this.logsLoading = true; try { const lines = Math.min(1000, Math.max(1, Number(this.logLines) || 200)); const [logs, activity] = await Promise.all([apiGet(this.auth, "logs", { lines, source: this.logSource }), apiGet(this.auth, "activity", { lines })]); if (this.disposed) return; this.logOutput = this.logsFrom(logs).slice(0, MAX_RESPONSE_BYTES); this.activityEvents = arrayOf(activity.events); this.logState = `Live · ${lines} line limit`; } catch (_error) { if (!this.disposed) this.logState = "Logs unavailable"; } finally { this.logsLoading = false; if (!this.disposed) this.scheduleLogs(); } },
    toggleLogs() { this.logsPaused = !this.logsPaused; this.logState = this.logsPaused ? "Paused" : "Resuming"; if (!this.logsPaused) this.refreshLogs(); else window.clearTimeout(this.logTimer); },
    clearLogView() { this.logOutput = "View cleared. The package log was not deleted."; },
    async saveNotificationPreferences(event) { if (event && event.preventDefault) event.preventDefault(); if (this.notificationForm.desktop_notifications && window.Notification && Notification.permission === "default") { const permission = await Notification.requestPermission(); if (permission !== "granted") this.notificationForm.desktop_notifications = false; } this.settings.desktop_notifications = this.notificationForm.desktop_notifications === true; this.settings.audible = this.notificationForm.audible === true; this.persistSettings(); this.toast("Session preferences saved", "These non-secret browser preferences are stored locally."); },
    saveInterfaceSettings(event) { if (event && event.preventDefault) event.preventDefault(); this.settings.status_refresh = Number(this.settings.status_refresh); this.settings.log_refresh = Number(this.settings.log_refresh); this.persistSettings(); this.scheduleSnapshot(); this.scheduleLogs(); this.toast("Interface settings saved", "Theme and refresh cadence were updated locally."); },
    persistSettings() { try { window.localStorage.setItem(SETTINGS_KEY, JSON.stringify(this.settings)); } catch (_error) { this.toast("Preferences not persisted", "Browser storage is unavailable for this DSM session.", true); } },
    maybeNotifyFailure() { if (this.runStatus !== "failed") return; const key = [this.run.profile || this.run.scope || "unknown", this.run.finished_epoch || this.run.started_epoch || "unknown", this.run.exit_code || "unknown"].join(":"); if (!this.lastFailureKey) { this.lastFailureKey = key; return; } if (key === this.lastFailureKey) return; this.lastFailureKey = key; if (this.settings.desktop_notifications && window.Notification && Notification.permission === "granted") new Notification("Synology Drive Sync failed", { body: "A newly observed package run failed. Open DSM for details.", icon: "/webman/3rdparty/synology-drive-sync/images/icon_64.png", tag: "sdsync-run-failure" }); if (this.settings.audible) this.playCue(); },
    playCue() { try { const AudioContext = window.AudioContext || window.webkitAudioContext; if (!AudioContext) return; const context = new AudioContext(); const oscillator = context.createOscillator(); const gain = context.createGain(); oscillator.frequency.value = 440; gain.gain.setValueAtTime(0.0001, context.currentTime); gain.gain.exponentialRampToValueAtTime(0.08, context.currentTime + 0.02); gain.gain.exponentialRampToValueAtTime(0.0001, context.currentTime + 0.18); oscillator.connect(gain); gain.connect(context.destination); oscillator.start(); oscillator.stop(context.currentTime + 0.2); oscillator.addEventListener("ended", () => context.close(), { once: true }); } catch (_error) { /* Best-effort local signal only. */ } },
    confirmationElement(reference) {
      const target = this.$refs[reference];
      if (!target) return null;
      const element = target.$el || target;
      if (element.matches && element.matches("button, [href], input, select, textarea, [tabindex]")) return element;
      return element.querySelector ? element.querySelector("button, [href], input, select, textarea, [tabindex]") : null;
    },
    confirmationFocusables() {
      const dialog = this.confirmationElement("confirmationDialog");
      if (!dialog || !dialog.querySelectorAll) return [];
      return Array.from(dialog.querySelectorAll("button, [href], input, select, textarea, [tabindex]"))
        .filter((element) => !element.disabled && element.getAttribute("tabindex") !== "-1" && element.getAttribute("aria-hidden") !== "true");
    },
    handleConfirmationKeydown(event) {
      if (!this.confirmation.visible) return;
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        this.settleConfirmation(false);
        return;
      }
      if (event.key !== "Tab") return;
      const dialog = this.confirmationElement("confirmationDialog");
      const focusable = this.confirmationFocusables();
      if (!dialog || !focusable.length) {
        event.preventDefault();
        if (dialog && dialog.focus) dialog.focus();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      const active = document.activeElement;
      if (event.shiftKey && (active === first || !dialog.contains(active))) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && (active === last || !dialog.contains(active))) {
        event.preventDefault();
        first.focus();
      }
    },
    removeConfirmationKeyHandler() {
      if (this.confirmationKeyHandler) {
        document.removeEventListener("keydown", this.confirmationKeyHandler, true);
        this.confirmationKeyHandler = null;
      }
    },
    confirmAction(title, message, button) {
      if (this.confirmation.resolve) this.settleConfirmation(false);
      this.confirmationPriorFocus = document.activeElement;
      this.confirmationKeyHandler = (event) => this.handleConfirmationKeydown(event);
      document.addEventListener("keydown", this.confirmationKeyHandler, true);
      return new Promise((resolve) => {
        this.confirmation = { visible: true, title, message, button, resolve };
        this.$nextTick(() => {
          if (!this.confirmation.visible || this.disposed) return;
          const initial = this.confirmationElement("confirmationCancel") || this.confirmationElement("confirmationDialog");
          if (initial && initial.focus) initial.focus();
        });
      });
    },
    settleConfirmation(accepted) {
      const resolve = this.confirmation.resolve;
      const priorFocus = this.confirmationPriorFocus;
      this.removeConfirmationKeyHandler();
      this.confirmationPriorFocus = null;
      this.confirmation = { visible: false, title: "", message: "", button: "Confirm", resolve: null };
      if (resolve) resolve(accepted);
      if (!this.disposed) {
        this.$nextTick(() => {
          if (priorFocus && priorFocus.isConnected && priorFocus.focus) priorFocus.focus();
        });
      }
    }
  }
};
</script>
