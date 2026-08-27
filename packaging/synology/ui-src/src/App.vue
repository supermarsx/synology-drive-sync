<template>
  <v-app-instance class-name="SYNO.SDS.App.SynologyDriveSync.Instance">
    <v-app-window
      ref="appWindow"
      syno-id="SYNO.SDS.App.SynologyDriveSync.Window"
      title="Synology Drive Sync"
      width="1180"
      height="760"
      :resizable="true"
    >
      <div class="sdsync-app" :class="themeClass">
        <aside class="sdsync-sidebar" aria-label="Application navigation">
          <div class="sdsync-brand">
            <img src="/webman/3rdparty/synology-drive-sync/images/icon_64.png" width="42" height="42" alt="">
            <div><strong>Drive Sync</strong><span>File Station sync</span></div>
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
              <span class="sdsync-nav-icon" aria-hidden="true"><action-icon :name="item.icon" :size="18" /></span>
              <span>{{ item.title }}</span>
            </button>
          </nav>
          <footer class="sdsync-sidebar-foot" aria-label="Package connection status">
            <span :class="['sdsync-connection-dot', { 'is-online': connected, 'is-error': !connected }]" />
            <span aria-live="polite">{{ connectionLabel }}</span>
          </footer>
        </aside>

        <main class="sdsync-workspace">
          <header class="sdsync-topbar">
            <div><h1 id="sdsync-page-title">{{ pageTitle }}</h1></div>
            <div class="sdsync-topbar-actions">
              <span class="sdsync-freshness" aria-live="polite">{{ freshness }}</span>
              <v-button
                type="border"
                display="icon-text"
                tooltip="Open this section in DSM Help"
                aria-label="Open Synology Drive Sync help in DSM Help"
                @click="openDsmHelp"
              ><template #icon><action-icon name="help" /></template>Help</v-button>
              <v-button
                type="border"
                display="icon-text"
                tooltip="Refresh current data"
                :disabled="snapshotLoading"
                @click="refreshSnapshot(true)"
              ><template #icon><action-icon name="refresh" /></template>Refresh</v-button>
            </div>
          </header>

          <div v-if="!canMutate" class="sdsync-banner" role="status">
            <div>
              <strong>{{ bridgeIssue.title || 'Read-only mode' }}</strong>
              <span>{{ bridgeIssue.message || 'Live status remains available, but changes stay disabled until the authenticated DSM bridge is ready.' }}</span>
              <v-button type="border" display="icon-text" tooltip="Retry DSM authentication and reload package status" :disabled="snapshotLoading" @click="refreshSnapshot(true)"><template #icon><action-icon name="refresh" /></template>Retry</v-button>
            </div>
          </div>

          <div class="sdsync-page-stage">
            <transition name="sdsync-page-swap" mode="out-in" appear>
              <div :key="route" class="sdsync-page-frame">
          <section v-if="route === 'overview'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <div class="sdsync-overview-status" aria-label="Service status and actions">
              <div class="sdsync-service-status">
                <span>Service</span>
                <strong :class="pillClass(serviceState)">{{ serviceState }}</strong>
                <small>{{ overviewSummary }}</small>
              </div>
              <div class="sdsync-action-row">
                <v-button suffix="grey" display="icon-text" tooltip="Preview every configured profile without changing destination files" :disabled="!canRunOperations || !profiles.length || operationBusy" @click="quickPlan"><template #icon><action-icon name="plan" /></template>Plan all profiles</v-button>
                <v-button suffix="main" display="icon-text" tooltip="Start a real sync for every configured profile with deletion disabled" :disabled="!canRunOperations || !profiles.length || operationBusy" @click="quickRun"><template #icon><action-icon name="run" /></template>Run all profiles</v-button>
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
                <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Destinations</p><h3>Profile readiness</h3></div><v-button type="styleless" display="icon-text" tooltip="Open profile configuration and protected credentials" @click="navigate('profiles')"><template #icon><action-icon name="navigate" /></template>Manage profiles</v-button></div>
                <p v-if="!profiles.length" class="sdsync-empty">No configured profiles.</p>
                <div v-for="profile in profiles" :key="profile.name" class="sdsync-compact-profile">
                  <div><strong>{{ profile.name }}</strong><span>{{ profile.remote || profile.remote_path || 'Destination unavailable' }}</span></div>
                  <span>{{ profile.has_password === true ? 'Credential stored' : 'Password required' }}</span>
                </div>
              </article>
              <article class="sdsync-panel">
                <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Recent state</p><h3>Last operation</h3></div><v-button type="styleless" display="icon-text" tooltip="Inspect structured events and bounded package logs" @click="navigate('activity')"><template #icon><action-icon name="navigate" /></template>Open activity</v-button></div>
                <dl class="sdsync-definition-grid">
                  <div><dt>Operation</dt><dd>{{ runOperation }}</dd></div><div><dt>State</dt><dd>{{ runStatus }}</dd></div>
                  <div><dt>Scope</dt><dd>{{ runScope }}</dd></div><div><dt>Started</dt><dd>{{ formatDate(run.started_epoch) }}</dd></div>
                  <div><dt>Finished</dt><dd>{{ formatDate(run.finished_epoch) }}</dd></div>
                </dl>
              </article>
            </div>
          </section>

          <section v-else-if="route === 'profiles'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <div class="sdsync-page-actions">
              <v-button suffix="main" display="icon-text" tooltip="Create a validated source-to-File-Station profile" :disabled="!canChangeProfiles || operationBusy" @click="openProfile('')"><template #icon><action-icon name="add" /></template>New profile</v-button>
            </div>
            <div :class="['sdsync-profiles-layout', { 'is-catalog-only': !profileEditorOpen }]">
              <div class="sdsync-panel sdsync-profile-catalog">
                <v-input class="sdsync-input-control" v-model="profileFilter" clearable placeholder="Filter profiles" aria-label="Filter profiles" aria-describedby="sdsync-help-profile-filter" /><control-help help-key="profile-filter" />
                <p v-if="!filteredProfiles.length" class="sdsync-empty">No configured profiles.</p>
                <button
                  v-for="profile in filteredProfiles"
                  :key="profile.name"
                  type="button"
                  :title="'Edit profile ' + profile.name"
                  :class="['sdsync-profile-row', { 'is-selected': selectedProfile === profile.name }]"
                  :disabled="operationBusy"
                  @click="openProfile(profile.name)"
                >
                  <span><strong><action-icon name="edit" />&nbsp;{{ profile.name }}</strong><span>{{ profile.remote || profile.remote_path || 'Destination unavailable' }}</span></span>
                  <span class="sdsync-badges"><i :class="['sdsync-mini-badge', { ready: profile.has_password === true }]">{{ profile.has_password === true ? 'Ready' : 'Needs password' }}</i><i v-if="profile.is_default === true || profile.default === true" class="sdsync-mini-badge">Default</i></span>
                </button>
              </div>

              <v-form v-if="profileEditorOpen" v-model="profileForm" class="sdsync-panel sdsync-editor" direction="vertical" @submit="saveProfile">
                <div class="sdsync-panel-heading">
                  <div><p class="sdsync-eyebrow">Profile editor</p><h3>{{ selectedProfile ? 'Edit ' + selectedProfile : 'New profile' }}</h3></div>
                  <v-button type="border" display="icon-text" tooltip="Close the editor and clear unsubmitted secret fields" @click="closeProfile"><template #icon><action-icon name="close" /></template>Close</v-button>
                </div>
                <div class="sdsync-form-grid">
                  <v-form-item class="sdsync-form-item" label="Name" prop="name"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-name" /></template><v-input class="sdsync-input-control" v-model.trim="profileForm.name" :readonly="Boolean(selectedProfile)" maxlength="64" placeholder="office_nas" aria-describedby="sdsync-help-profile-name" :disabled="!canChangeProfiles" /></v-form-item>
                  <v-form-item class="sdsync-form-item" label="Local source" prop="source"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-source" /></template><v-input class="sdsync-input-control" v-model.trim="profileForm.source" placeholder="/volume1/Source" aria-describedby="sdsync-help-profile-source" :disabled="!canChangeProfiles" /></v-form-item>
                  <v-form-item class="sdsync-form-item span-2" label="File Station URL" prop="url"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-url" /></template><v-input class="sdsync-input-control" v-model.trim="profileForm.url" placeholder="https://files.example.com" aria-describedby="sdsync-help-profile-url" :disabled="!canChangeProfiles" /></v-form-item>
                  <v-form-item class="sdsync-form-item" label="DSM username" prop="username"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-username" /></template><v-input class="sdsync-input-control" v-model.trim="profileForm.username" autocomplete="username" aria-describedby="sdsync-help-profile-username" :disabled="!canChangeProfiles" /></v-form-item>
                  <v-form-item class="sdsync-form-item" label="Remote logical path" prop="remote"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-remote" /></template><v-input class="sdsync-input-control" v-model.trim="profileForm.remote" placeholder="/home/Drive/NAS Backup" aria-describedby="sdsync-help-profile-remote" :disabled="!canChangeProfiles" /></v-form-item>
                  <v-form-item class="sdsync-form-item" label="Comparison"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-compare" /></template><v-single-select class="sdsync-select-control" v-model="profileForm.compare" :options="compareOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-profile-compare" :disabled="!canChangeProfiles"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                  <v-form-item class="sdsync-form-item" label="Concurrent uploads"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-jobs" /></template><v-input class="sdsync-input-control" v-model="profileForm.jobs" number-only aria-describedby="sdsync-help-profile-jobs" :disabled="!canChangeProfiles" /></v-form-item>
                  <div class="sdsync-check-row span-2"><v-checkbox class="sdsync-checkbox-control" v-model="profileForm.allow_http" aria-describedby="sdsync-help-profile-http" :disabled="!canEditHttpException">Allow plain HTTP for controlled LAN testing</v-checkbox><control-help help-key="profile-http" /></div>
                </div>

                <fieldset class="sdsync-danger-fieldset">
                  <legend>Deletion guard</legend>
                  <div class="sdsync-check-row"><v-checkbox class="sdsync-checkbox-control" v-model="profileForm.delete" aria-describedby="sdsync-help-profile-delete" :disabled="!canEditProfileDeletion">Mirror remote deletions after profile and run-level approval</v-checkbox><control-help help-key="profile-delete" /></div>
                  <v-form-item class="sdsync-form-item" label="Maximum deletions per run"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-max-delete" /></template><v-input class="sdsync-input-control" v-model="profileForm.max_delete" number-only aria-describedby="sdsync-help-profile-max-delete" :disabled="!canChangeProfiles" /></v-form-item>
                </fieldset>

                <details class="sdsync-advanced">
                  <summary><strong><action-icon name="settings" />&nbsp;Advanced profile controls</strong><span>Network, retry, output, and remote observability policy</span></summary>
                  <div class="sdsync-form-grid">
                    <v-form-item class="sdsync-form-item span-2" label="Excludes"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-excludes" /></template><v-input class="sdsync-input-control" v-model="profileForm.excludes" type="textarea" :autosize="{ minRows: 3, maxRows: 7 }" placeholder="@eaDir/&#10;**/@eaDir/&#10;#recycle/" aria-describedby="sdsync-help-profile-excludes" :disabled="!canChangeProfiles" /></v-form-item>
                    <div class="sdsync-check-row span-2"><v-checkbox class="sdsync-checkbox-control" v-model="profileForm.allow_empty_source" aria-describedby="sdsync-help-profile-empty-source" :disabled="!canEditEmptySourceException">Allow an empty source (disables the empty-source deletion guard)</v-checkbox><control-help help-key="profile-empty-source" /></div>
                    <v-form-item class="sdsync-form-item" label="Retries"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-retries" /></template><v-input class="sdsync-input-control" v-model="profileForm.retries" number-only aria-describedby="sdsync-help-profile-retries" :disabled="!canChangeProfiles" /></v-form-item>
                    <v-form-item class="sdsync-form-item" label="Upload timeout (seconds)"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-timeout" /></template><v-input class="sdsync-input-control" v-model="profileForm.timeout" number-only aria-describedby="sdsync-help-profile-timeout" :disabled="!canChangeProfiles" /></v-form-item>
                    <v-form-item class="sdsync-form-item" label="Connect timeout (seconds)"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-connect-timeout" /></template><v-input class="sdsync-input-control" v-model="profileForm.connect_timeout" number-only aria-describedby="sdsync-help-profile-connect-timeout" :disabled="!canChangeProfiles" /></v-form-item>
                    <v-form-item class="sdsync-form-item" label="Maximum rate (bytes/s)"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-rate" /></template><v-input class="sdsync-input-control" v-model="profileForm.max_rate" number-only aria-describedby="sdsync-help-profile-rate" :disabled="!canChangeProfiles" /></v-form-item>
                    <v-form-item class="sdsync-form-item span-2" label="CA certificate path"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-ca" /></template><v-input class="sdsync-input-control" v-model.trim="profileForm.ca_certificate" placeholder="/volume1/certificates/ca.pem" aria-describedby="sdsync-help-profile-ca" :disabled="!canChangeProfiles" /></v-form-item>
                    <div class="sdsync-check-row span-2"><v-checkbox class="sdsync-checkbox-control" v-model="profileForm.danger_invalid_certs" aria-describedby="sdsync-help-profile-invalid-certs" :disabled="!canEditInvalidTlsException">Accept invalid TLS certificates (unsafe)</v-checkbox><control-help help-key="profile-invalid-certs" /></div>
                    <div v-if="profileForm.danger_invalid_certs" class="sdsync-check-row span-2"><v-checkbox class="sdsync-checkbox-control" v-model="profileForm.danger_invalid_confirm" label-color="red" aria-describedby="sdsync-help-profile-invalid-confirm" :disabled="!canEditInvalidTlsException">I accept the interception risk</v-checkbox><control-help help-key="profile-invalid-confirm" /></div>
                    <v-form-item class="sdsync-form-item" label="Verbosity"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-verbosity" /></template><v-single-select class="sdsync-select-control" v-model="profileForm.verbosity" :options="verbosityOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-profile-verbosity" :disabled="!canChangeProfiles"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                    <div class="sdsync-check-row"><v-checkbox class="sdsync-checkbox-control" v-model="profileForm.quiet" aria-describedby="sdsync-help-profile-quiet" :disabled="!canChangeProfiles">Quiet terminal sink; durable logs remain active</v-checkbox><control-help help-key="profile-quiet" /></div>
                    <v-form-item class="sdsync-form-item" label="Log level"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-log-level" /></template><v-single-select class="sdsync-select-control" v-model="profileForm.log_level" :options="logLevelOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-profile-log-level" :disabled="!canChangeProfiles"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                    <v-form-item class="sdsync-form-item" label="Log format" textonly><span>JSON · package managed</span></v-form-item>
                    <v-form-item class="sdsync-form-item" label="Progress" textonly><span>Never · package managed</span></v-form-item>
                    <v-form-item class="sdsync-form-item" label="Output" textonly><span>Human · package managed</span></v-form-item>
                    <v-form-item class="sdsync-form-item span-2" label="Remote log URL"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-log-url" /></template><v-input class="sdsync-input-control" v-model.trim="profileForm.remote_log_url" placeholder="https://collector.example.com/ingest" aria-describedby="sdsync-help-profile-log-url" :disabled="!canEditRemoteLogging" /></v-form-item>
                    <v-form-item class="sdsync-form-item" label="Remote log mode"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-log-mode" /></template><v-single-select class="sdsync-select-control" v-model="profileForm.remote_log_mode" :options="remoteLogModeOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-profile-log-mode" :disabled="!canEditRemoteLogging"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                  </div>
                  <div class="sdsync-secret-editor">
                    <div><strong>Remote log token</strong><span>{{ selectedProfileModel && selectedProfileModel.has_remote_log_token ? 'Stored · masked' : 'Not stored' }}</span></div>
                    <v-single-select class="sdsync-select-control" v-model="secretModes.remote_log_token" :options="secretModeOptions" width="210" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-secret-log-mode" :disabled="!canManageSecrets || !canAllowRemoteLogging"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select><control-help help-key="secret-log-mode" />
                    <v-input class="sdsync-input-control" v-if="secretModes.remote_log_token === 'replace'" v-model="secretValues.remote_log_token" type="password" maxlength="4096" autocomplete="new-password" placeholder="New token" aria-describedby="sdsync-help-secret-log-value" :disabled="!canManageSecrets || !canAllowRemoteLogging" /><control-help v-if="secretModes.remote_log_token === 'replace'" help-key="secret-log-value" />
                  </div>
                </details>

                <fieldset class="sdsync-secret-fieldset">
                  <legend>Protected credentials</legend>
                  <div class="sdsync-secret-editor">
                    <div><strong>Password</strong><span>{{ selectedProfileModel && selectedProfileModel.has_password ? 'Stored · masked' : 'Not stored' }}</span></div>
                    <v-single-select class="sdsync-select-control" v-model="secretModes.password" :options="secretModeOptions" width="210" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-secret-password-mode" :disabled="!canManageSecrets"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select><control-help help-key="secret-password-mode" />
                    <v-input class="sdsync-input-control" v-if="secretModes.password === 'replace'" v-model="secretValues.password" type="password" maxlength="4096" autocomplete="new-password" placeholder="New password" aria-describedby="sdsync-help-secret-password-value" :disabled="!canManageSecrets" /><control-help v-if="secretModes.password === 'replace'" help-key="secret-password-value" />
                  </div>
                  <div class="sdsync-secret-editor">
                    <div><strong>TOTP seed</strong><span>{{ selectedProfileModel && selectedProfileModel.has_totp ? 'Stored · masked' : 'Not stored' }}</span></div>
                    <v-single-select class="sdsync-select-control" v-model="secretModes.totp" :options="secretModeOptions" width="210" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-secret-totp-mode" :disabled="!canManageSecrets"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select><control-help help-key="secret-totp-mode" />
                    <v-input class="sdsync-input-control" v-if="secretModes.totp === 'replace'" v-model="secretValues.totp" type="password" maxlength="4096" autocomplete="off" placeholder="Base32 seed or otpauth URI" aria-describedby="sdsync-help-secret-totp-value" :disabled="!canManageSecrets" /><control-help v-if="secretModes.totp === 'replace'" help-key="secret-totp-value" />
                  </div>
                  <p class="sdsync-field-note">Secret values are sent only in the protected request body. They are never returned to this window.</p>
                </fieldset>

                <div class="sdsync-check-row"><v-checkbox class="sdsync-checkbox-control" v-model="profileForm.make_default" aria-describedby="sdsync-help-profile-default" :disabled="!canChangeProfiles">Use as default profile</v-checkbox><control-help help-key="profile-default" /></div>
                <div class="sdsync-form-actions">
                  <v-button v-if="selectedProfile" suffix="red" display="icon-text" tooltip="Remove package configuration and stored credentials, not synchronized files" :disabled="!canChangeProfiles || operationBusy" @click="removeProfile"><template #icon><action-icon name="delete" /></template>Delete profile</v-button>
                  <span />
                  <v-button suffix="cancel" display="icon-text" tooltip="Discard unsaved editor values and clear secret fields" @click="closeProfile"><template #icon><action-icon name="close" /></template>Cancel</v-button>
                  <v-button suffix="main" display="icon-text" html-type="submit" tooltip="Validate and apply configuration, then process secret operations" :disabled="!canChangeProfiles || operationBusy"><template #icon><action-icon name="save" /></template>Save profile</v-button>
                </div>
              </v-form>
            </div>
          </section>

          <section v-else-if="route === 'routines'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <div class="sdsync-subtabs" data-subtabs="routines" role="tablist" aria-label="Routine views" @keydown="moveSubtab('routineTab', routineTabs, $event)">
              <button v-for="tab in routineTabs" :id="'sdsync-routines-tab-' + tab.id" :key="tab.id" type="button" :class="['sdsync-subtab', { 'is-active': routineTab === tab.id }]" :data-subtab="tab.id" role="tab" :aria-selected="routineTab === tab.id" :aria-controls="'sdsync-routines-panel-' + tab.id" :tabindex="routineTab === tab.id ? 0 : -1" @click="routineTab = tab.id">{{ tab.label }}</button>
            </div>
            <div class="sdsync-subtab-stage">
              <transition name="sdsync-subtab-swap" mode="out-in">
                <div v-if="routineTab === 'configured-profiles'" id="sdsync-routines-panel-configured-profiles" key="configured-profiles" class="sdsync-subtab-panel" data-subtab-panel="configured-profiles" role="tabpanel" aria-labelledby="sdsync-routines-tab-configured-profiles" tabindex="0">
                  <article class="sdsync-panel">
                    <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Configured routines</p><h3>Per-profile automation</h3></div></div>
                    <p v-if="!routines.length" class="sdsync-empty">No configured routines.</p>
                    <button v-for="routine in routines" :key="routine.profile" type="button" class="sdsync-routine-row" :title="'Edit routine for ' + routine.profile" :disabled="operationBusy" @click="selectRoutine(routine.profile)"><span><strong><action-icon name="edit" />&nbsp;{{ routine.profile }}</strong><small>{{ routine.mode || 'interval' }} · {{ routine.backend || 'fallback unreported' }} · {{ routine.state || (routine.enabled ? 'enabled' : 'disabled') }}</small></span><time>{{ routine.enabled ? formatDate(routine.next_run_epoch) : 'Disabled' }}</time></button>
                  </article>
                </div>
                <div v-else id="sdsync-routines-panel-package-controller" key="package-controller" class="sdsync-subtab-panel" data-subtab-panel="package-controller" role="tabpanel" aria-labelledby="sdsync-routines-tab-package-controller" tabindex="0">
                  <v-form v-model="routineForm" class="sdsync-panel" direction="vertical" @submit="saveRoutine">
                    <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Routine editor</p><h3>Profile automation policy</h3></div><span :class="pillClass(selectedRoutine ? selectedRoutine.state : 'unknown')">{{ selectedRoutine ? (selectedRoutine.state || (selectedRoutine.enabled ? 'Enabled' : 'Disabled')) : 'New' }}</span></div>
                    <v-form-item class="sdsync-form-item" label="Profile"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-profile" /></template><v-single-select class="sdsync-select-control" v-model="routineForm.profile" :options="profileOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-routine-profile" :disabled="!canChangeRoutines || operationBusy" @input="loadRoutine"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                    <div class="sdsync-check-row"><v-checkbox class="sdsync-checkbox-control" v-model="routineForm.enabled" aria-describedby="sdsync-help-routine-enabled" :disabled="!canChangeRoutines">Enable routine</v-checkbox><control-help help-key="routine-enabled" /></div>
                    <div class="sdsync-form-grid compact">
                      <v-form-item class="sdsync-form-item" label="Action"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-action" /></template><v-single-select class="sdsync-select-control" v-model="routineForm.action" :options="routineActionOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-routine-action" :disabled="!canChangeRoutines"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                      <v-form-item class="sdsync-form-item" label="Mode"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-mode" /></template><v-single-select class="sdsync-select-control" v-model="routineForm.mode" :options="routineModeOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-routine-mode" :disabled="!canChangeRoutines"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                      <v-form-item class="sdsync-form-item" label="Interval (seconds)"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-interval" /></template><v-input class="sdsync-input-control" v-model="routineForm.interval_seconds" number-only aria-describedby="sdsync-help-routine-interval" :disabled="!canChangeRoutines" /></v-form-item>
                      <v-form-item class="sdsync-form-item" label="Window starts"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-window-start" /></template><input v-model="routineForm.time_window_start" class="sdsync-native-input" type="time" aria-label="Window starts" aria-describedby="sdsync-help-routine-window-start" :disabled="!canChangeRoutines"></v-form-item>
                      <v-form-item class="sdsync-form-item" label="Window ends"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-window-end" /></template><input v-model="routineForm.time_window_end" class="sdsync-native-input" type="time" aria-label="Window ends" aria-describedby="sdsync-help-routine-window-end" :disabled="!canChangeRoutines"></v-form-item>
                      <v-form-item class="sdsync-form-item" label="Realtime debounce (seconds)"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-debounce" /></template><v-input class="sdsync-input-control" v-model="routineForm.debounce_seconds" number-only aria-describedby="sdsync-help-routine-debounce" :disabled="!canChangeRoutines" /></v-form-item>
                      <v-form-item class="sdsync-form-item" label="Fallback poll (seconds)"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-poll" /></template><v-input class="sdsync-input-control" v-model="routineForm.poll_seconds" number-only aria-describedby="sdsync-help-routine-poll" :disabled="!canChangeRoutines" /></v-form-item>
                      <v-form-item class="sdsync-form-item" label="Retry attempts"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-retries" /></template><v-input class="sdsync-input-control" v-model="routineForm.retry_count" number-only aria-describedby="sdsync-help-routine-retries" :disabled="!canChangeRoutines" /></v-form-item>
                      <v-form-item class="sdsync-form-item" label="Retry backoff (seconds)"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-backoff" /></template><v-input class="sdsync-input-control" v-model="routineForm.retry_backoff_seconds" number-only aria-describedby="sdsync-help-routine-backoff" :disabled="!canChangeRoutines" /></v-form-item>
                      <v-form-item class="sdsync-form-item span-2" label="Wait for routines">
                        <template #label-after><control-help class="sdsync-form-label-help" help-key="routine-dependencies" /></template>
                        <select v-model="routineForm.depends_on" class="sdsync-native-input" multiple size="4" aria-label="Wait for routines" aria-describedby="sdsync-help-routine-dependencies" :disabled="!canChangeRoutines">
                          <option v-for="profile in dependencyProfiles" :key="profile.name" :value="profile.name">{{ profile.name }}</option>
                        </select>
                      </v-form-item>
                    </div>
                    <fieldset class="sdsync-weekday-fieldset" aria-describedby="sdsync-help-routine-weekdays" :disabled="!canChangeRoutines"><legend>Active weekdays <control-help help-key="routine-weekdays" /></legend><div class="sdsync-weekdays"><label v-for="day in weekdayOptions" :key="day.value"><input v-model="routineForm.weekdays" type="checkbox" :value="day.value" :disabled="!canChangeRoutines"><span>{{ day.label }}</span></label></div></fieldset>
                    <fieldset class="sdsync-danger-fieldset"><legend>Routine deletion guard</legend><div class="sdsync-check-row"><v-checkbox class="sdsync-checkbox-control" v-model="routineForm.allow_delete" aria-describedby="sdsync-help-routine-delete" :disabled="!canEditRoutineDeletion">Permit profile deletion rules</v-checkbox><control-help help-key="routine-delete" /></div><v-form-item class="sdsync-form-item" label="Routine deletion approval ceiling"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-max-delete" /></template><v-input class="sdsync-input-control" v-model="routineForm.max_total_delete" number-only aria-describedby="sdsync-help-routine-max-delete" :disabled="!canChangeRoutines" /></v-form-item></fieldset>
                    <div class="sdsync-form-actions"><v-button suffix="red" display="icon-text" tooltip="Remove this automation policy without deleting its profile" :disabled="!canChangeRoutines || !selectedRoutine || operationBusy" @click="removeRoutine"><template #icon><action-icon name="delete" /></template>Remove routine</v-button><span /><v-button suffix="main" display="icon-text" html-type="submit" tooltip="Validate and apply this per-profile automation policy" :disabled="!canChangeRoutines || !routineForm.profile || operationBusy"><template #icon><action-icon name="save" /></template>Save routine</v-button></div>
                  </v-form>
                </div>
              </transition>
            </div>
          </section>

          <section v-else-if="route === 'health'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <div class="sdsync-two-column">
              <v-form v-model="doctorForm" class="sdsync-panel" direction="vertical" @submit="runDoctor">
                <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Target doctor</p><h3>Run a diagnostic</h3></div><span class="sdsync-pill neutral">Manual</span></div>
                <v-form-item class="sdsync-form-item" label="Scope"><template #label-after><control-help class="sdsync-form-label-help" help-key="doctor-scope" /></template><v-single-select class="sdsync-select-control" v-model="doctorForm.scope" :options="scopeOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-doctor-scope" :disabled="!canRunOperations"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                <div class="sdsync-check-row"><v-checkbox class="sdsync-checkbox-control" v-model="doctorForm.write_test" aria-describedby="sdsync-help-doctor-write" :disabled="!canRunOperations || !canRunDoctorWrite || !hasCapability('write_test')">Disposable write test</v-checkbox><control-help help-key="doctor-write" /></div>
                <div v-if="doctorForm.write_test" class="sdsync-warning"><strong>This mutates the selected target briefly.</strong><div class="sdsync-check-row"><v-checkbox class="sdsync-checkbox-control" v-model="doctorForm.write_confirm" aria-describedby="sdsync-help-doctor-write-confirm" :disabled="!canRunOperations || !canRunDoctorWrite">I prepared a non-critical destination and approve probe cleanup.</v-checkbox><control-help help-key="doctor-write-confirm" /></div></div>
                <v-button suffix="main" display="icon-text" html-type="submit" tooltip="Run preflight checks and wait for bounded terminal Doctor evidence" :disabled="!canRunOperations || operationBusy"><template #icon><action-icon name="doctor" /></template>Run doctor</v-button>
              </v-form>
              <article class="sdsync-panel sdsync-diagnostic" aria-live="polite"><div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Latest diagnostic</p><h3>{{ diagnostic.title }}</h3></div><span class="sdsync-pulse" /></div><pre>{{ diagnostic.output }}</pre></article>
            </div>
            <article class="sdsync-panel"><div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Cached per-profile evidence</p><h3>Target health</h3></div><span class="sdsync-freshness">{{ healthFreshness }}</span></div><div class="sdsync-table-wrap"><table><thead><tr><th>Profile</th><th>Last check</th><th>Reachable</th><th>Auth</th><th>Writable</th><th>Latency</th><th>Last success</th><th>Doctor</th><th>Free space</th></tr></thead><tbody><tr v-if="!healthRows.length"><td colspan="9">No cached target-health evidence.</td></tr><tr v-for="health in healthRows" :key="health.profile"><td>{{ health.profile || 'Unknown' }}</td><td>{{ formatDate(health.last_check_epoch || health.checked_at_epoch || health.checked_epoch) }}</td><td :class="healthClass(health.reachable)">{{ booleanEvidence(health.reachable) }}</td><td :class="healthClass(health.authenticated !== undefined ? health.authenticated : health.auth)">{{ booleanEvidence(health.authenticated !== undefined ? health.authenticated : health.auth) }}</td><td :class="healthClass(health.writable)">{{ booleanEvidence(health.writable) }}</td><td>{{ formatDuration(health.latency_ms) }}</td><td>{{ formatDate(health.last_success_epoch || health.last_successful_sync_epoch) }}</td><td>{{ health.doctor_status || health.last_doctor_status || health.state || 'Unavailable' }}</td><td>{{ health.free_space_proven === true ? formatBytes(health.free_space_bytes) : 'Unavailable' }}</td></tr></tbody></table></div></article>
          </section>

          <section v-else-if="route === 'activity'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <div class="sdsync-page-actions"><v-button suffix="grey" display="icon-text" tooltip="Pause or resume browser-side log refreshes" @click="toggleLogs"><template #icon><action-icon :name="logsPaused ? 'run' : 'pause'" /></template>{{ logsPaused ? 'Resume live updates' : 'Pause live updates' }}</v-button><v-button suffix="grey" display="icon-text" tooltip="Clear only this rendered view; package logs remain intact" @click="clearLogView"><template #icon><action-icon name="clear" /></template>Clear view</v-button></div>
            <article class="sdsync-panel">
              <div class="sdsync-panel-heading">
                <div><p class="sdsync-eyebrow">Structured activity</p><h3>Recent package events</h3></div>
                <span class="sdsync-freshness">{{ reversedActivity.length }} of {{ activityEvents.length }} event{{ activityEvents.length === 1 ? '' : 's' }}</span>
              </div>
              <div class="sdsync-log-toolbar" aria-label="Activity filters">
                <v-input v-model.trim="activitySearch" class="sdsync-input-control sdsync-activity-search" maxlength="128" placeholder="Search event text or request ID" aria-label="Search activity text or client request ID" aria-describedby="sdsync-help-activity-search" /><control-help help-key="activity-search" />
                <v-single-select class="sdsync-select-control" v-model="activityCategory" :options="activityCategoryOptions" width="190" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-activity-category"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select><control-help help-key="activity-category" />
                <v-single-select class="sdsync-select-control" v-model="activityLevel" :options="activityLevelOptions" width="160" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-activity-level"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select><control-help help-key="activity-level" />
              </div>
              <ol class="sdsync-activity-feed"><li v-if="!reversedActivity.length" class="sdsync-empty">No package events match these filters.</li><li v-for="event in reversedActivity" :key="[event.epoch, event.code, event.profile, event.category, event.level, event.client_request_id].join(':')"><time>{{ formatDate(event.epoch) }}</time><div class="sdsync-activity-detail"><strong>{{ event.code }}</strong><p v-if="event.message">{{ event.message }}</p><code v-if="event.client_request_id">Client request ID: {{ event.client_request_id }}</code></div><small>{{ event.profile }} · {{ event.state }} · {{ event.category }} / {{ event.level }}</small></li></ol>
            </article>
            <article class="sdsync-panel sdsync-log-panel"><div class="sdsync-log-toolbar"><v-single-select class="sdsync-select-control" v-model="logSource" :options="logSourceOptions" width="180" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-log-source" @input="refreshLogs"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select><control-help help-key="log-source" /><v-single-select class="sdsync-select-control" v-model="logLines" :options="logLineOptions" width="150" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-log-lines" @input="refreshLogs"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select><control-help help-key="log-lines" /><span>{{ logState }}</span></div><pre tabindex="0">{{ logOutput }}</pre></article>
          </section>

          <section v-else-if="route === 'notifications'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <div class="sdsync-subtabs" data-subtabs="notifications" role="tablist" aria-label="Notification settings" @keydown="moveSubtab('notificationTab', notificationTabs, $event)">
              <button v-for="tab in notificationTabs" :id="'sdsync-notifications-tab-' + tab.id" :key="tab.id" type="button" :class="['sdsync-subtab', { 'is-active': notificationTab === tab.id }]" :data-subtab="tab.id" role="tab" :aria-selected="notificationTab === tab.id" :aria-controls="'sdsync-notifications-panel-' + tab.id" :tabindex="notificationTab === tab.id ? 0 : -1" @click="notificationTab = tab.id">{{ tab.label }}</button>
            </div>
            <div class="sdsync-subtab-stage">
              <transition name="sdsync-subtab-swap" mode="out-in">
                <div v-if="notificationTab === 'package-alerts'" id="sdsync-notifications-panel-package-alerts" key="package-alerts" class="sdsync-subtab-panel" data-subtab-panel="package-alerts" role="tabpanel" aria-labelledby="sdsync-notifications-tab-package-alerts" tabindex="0">
                  <v-form v-model="alertForm" class="sdsync-panel" direction="vertical" @submit="saveAlerts">
                    <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">DSM desktop alerts</p><h3>Package alert policy</h3></div><span :class="pillClass(alertForm.enabled ? 'running' : 'disabled')">{{ alertForm.enabled ? 'Enabled' : 'Disabled' }}</span></div>
                    <div class="sdsync-check-row"><v-checkbox class="sdsync-checkbox-control" v-model="alertForm.enabled" aria-describedby="sdsync-help-alerts-enabled" :disabled="!canChangeNotifications">Enable DSM desktop alerts</v-checkbox><control-help help-key="alerts-enabled" /></div>
                    <div class="sdsync-check-row"><v-checkbox class="sdsync-checkbox-control" v-model="alertForm.on_success" aria-describedby="sdsync-help-alerts-success" :disabled="!canChangeNotifications">Notify on success</v-checkbox><control-help help-key="alerts-success" /></div>
                    <div class="sdsync-check-row"><v-checkbox class="sdsync-checkbox-control" v-model="alertForm.on_failure" aria-describedby="sdsync-help-alerts-failure" :disabled="!canChangeNotifications">Notify on failure</v-checkbox><control-help help-key="alerts-failure" /></div>
                    <v-form-item class="sdsync-form-item" label="Failures before alert"><template #label-after><control-help class="sdsync-form-label-help" help-key="alerts-threshold" /></template><v-input class="sdsync-input-control" v-model="alertForm.failure_threshold" number-only :disabled="!canChangeNotifications" aria-describedby="sdsync-help-alerts-threshold" /></v-form-item>
                    <v-form-item class="sdsync-form-item" label="Cooldown (seconds)"><template #label-after><control-help class="sdsync-form-label-help" help-key="alerts-cooldown" /></template><v-input class="sdsync-input-control" v-model="alertForm.cooldown_seconds" number-only :disabled="!canChangeNotifications" aria-describedby="sdsync-help-alerts-cooldown" /></v-form-item>
                    <v-button suffix="main" display="icon-text" html-type="submit" tooltip="Validate and persist the package-level DSM alert policy" :disabled="!canChangeNotifications || operationBusy"><template #icon><action-icon name="save" /></template>Save DSM alert policy</v-button>
                  </v-form>
                </div>
                <div v-else id="sdsync-notifications-panel-session-preferences" key="session-preferences" class="sdsync-subtab-panel" data-subtab-panel="session-preferences" role="tabpanel" aria-labelledby="sdsync-notifications-tab-session-preferences" tabindex="0">
                  <v-form v-model="notificationForm" class="sdsync-panel" direction="vertical" @submit="saveNotificationPreferences">
                    <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Open-session signal</p><h3>Browser fallback</h3></div><span :class="pillClass(notificationPermission)">{{ notificationPermission }}</span></div>
                    <div class="sdsync-check-row"><v-checkbox class="sdsync-checkbox-control" v-model="notificationForm.desktop_notifications" aria-describedby="sdsync-help-session-notify" :disabled="!canChangeNotifications">Notify while this app is open</v-checkbox><control-help help-key="session-notify" /></div>
                    <div class="sdsync-check-row"><v-checkbox class="sdsync-checkbox-control" v-model="notificationForm.audible" aria-describedby="sdsync-help-session-audible" :disabled="!canChangeNotifications">Audible cue</v-checkbox><control-help help-key="session-audible" /></div>
                    <v-button suffix="grey" display="icon-text" html-type="submit" tooltip="Save and audit non-secret notification preferences in this browser" :disabled="!canChangeNotifications || operationBusy"><template #icon><action-icon name="save" /></template>Save session preferences</v-button>
                  </v-form>
                </div>
              </transition>
            </div>
          </section>

          <section v-else-if="route === 'security'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <security-panel :value="securityForm" :disabled="!canMutate" :busy="operationBusy" :dirty="securityDirty" :theme-class="themeClass" :log-level-options="logLevelOptions" @input="updateSecurityForm" @save="saveSecurityPolicy" />
          </section>

          <section v-else-if="route === 'settings'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <v-form v-model="settings" class="sdsync-panel sdsync-settings-panel" direction="horizontal" @submit="saveInterfaceSettings"><div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Display and refresh</p><h3>Interface</h3></div></div><v-form-item class="sdsync-form-item" label="Theme"><template #label-after><control-help class="sdsync-form-label-help" help-key="settings-theme" /></template><v-single-select class="sdsync-select-control" v-model="settings.theme" :options="themeOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-settings-theme" :disabled="!canChangeInterface"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item><v-form-item class="sdsync-form-item" label="Status refresh"><template #label-after><control-help class="sdsync-form-label-help" help-key="settings-status-refresh" /></template><v-single-select class="sdsync-select-control" v-model="settings.status_refresh" :options="statusRefreshOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-settings-status-refresh" :disabled="!canChangeInterface"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item><v-form-item class="sdsync-form-item" label="Log refresh"><template #label-after><control-help class="sdsync-form-label-help" help-key="settings-log-refresh" /></template><v-single-select class="sdsync-select-control" v-model="settings.log_refresh" :options="logRefreshOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-settings-log-refresh" :disabled="!canChangeInterface"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item><div class="sdsync-form-actions sdsync-settings-actions"><span /><span /><v-button suffix="main" display="icon-text" html-type="submit" tooltip="Apply, persist, and audit this browser's AppWindow preferences" :disabled="!canChangeInterface || operationBusy"><template #icon><action-icon name="save" /></template>Save interface settings</v-button></div></v-form>
          </section>

          <section v-else-if="route === 'about'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <div class="sdsync-about-grid">
              <section class="sdsync-panel">
                <div class="sdsync-panel-heading"><h3>Build identity</h3></div>
                <dl class="sdsync-about-facts">
                  <div><dt>Project</dt><dd><code>{{ aboutMetadata.project }}</code></dd></div>
                  <div><dt>Author</dt><dd><a :href="aboutMetadata.authorUrl" target="_blank" rel="noopener noreferrer">{{ aboutMetadata.author }}</a></dd></div>
                  <div><dt>Maintainer</dt><dd><a :href="aboutMetadata.maintainerUrl" target="_blank" rel="noopener noreferrer">{{ aboutMetadata.maintainer }}</a></dd></div>
                  <div><dt>Repository</dt><dd><a :href="aboutMetadata.repository" target="_blank" rel="noopener noreferrer">GitHub</a></dd></div>
                  <div><dt>License</dt><dd><a :href="aboutMetadata.licenseUrl" target="_blank" rel="noopener noreferrer">{{ aboutMetadata.license }}</a></dd></div>
                  <div><dt>Installed package version</dt><dd><code>{{ installedPackageVersion }}</code></dd></div>
                  <div><dt>Core source version</dt><dd><code>{{ aboutMetadata.coreVersion }}</code></dd></div>
                  <div><dt>DSM UI build version</dt><dd><code>{{ aboutMetadata.uiVersion }}</code></dd></div>
                  <div><dt>API schema</dt><dd><code>{{ aboutMetadata.apiSchema }}</code></dd></div>
                </dl>
              </section>
              <section class="sdsync-panel">
                <div class="sdsync-panel-heading"><h3>Updates</h3></div>
                <div class="sdsync-update-links">
                  <a :href="aboutMetadata.releasesUrl" target="_blank" rel="noopener noreferrer">GitHub Releases</a>
                  <a :href="aboutMetadata.releaseSelectorUrl" target="_blank" rel="noopener noreferrer">Compatible release selector</a>
                </div>
                <p>Select the exact SPK for the NAS model, DSM version, and CPU architecture, then verify its checksum.</p>
                <p>Upgrade with Package Center <strong>Manual Install</strong>. Package lifecycle scripts retain configuration and protected secrets during an upgrade.</p>
                <p class="sdsync-field-note">This AppWindow does not fetch or install updates and does not configure Package Source discovery.</p>
              </section>
              <section class="sdsync-panel">
                <div class="sdsync-panel-heading"><h3>Direct Rust dependencies</h3></div>
                <p class="sdsync-field-note">Exact direct versions resolved by the frozen <code>Cargo.lock</code> for this build.</p>
                <ul class="sdsync-dependency-list"><li v-for="dependency in aboutRustDependencies" :key="dependency.name"><span><a :href="dependency.url" target="_blank" rel="noopener noreferrer">{{ dependency.name }}</a><small>{{ dependency.scope }}</small></span><code>{{ dependency.pin }}</code></li></ul>
              </section>
              <section class="sdsync-panel">
                <div class="sdsync-panel-heading"><h3>DSM UI build dependencies</h3></div>
                <ul class="sdsync-dependency-list"><li v-for="dependency in aboutUiDependencies" :key="dependency.name"><span><a :href="dependency.url" target="_blank" rel="noopener noreferrer">{{ dependency.name }}</a><small>{{ dependency.scope }}</small></span><code>{{ dependency.pin }}</code></li></ul>
                <p class="sdsync-field-note"><code>THIRD_PARTY_LICENSES.html</code> contains the complete transitive Rust release-dependency license inventory. Notices for runtime code embedded in this AppWindow bundle ship as <code>DSM_UI_THIRD_PARTY_LICENSES.txt</code>. Vue is supplied by DSM and is not bundled; other pnpm packages whose code is not named in that notice are used only during the build.</p>
              </section>
            </div>
          </section>
              </div>
            </transition>
          </div>
        </main>

        <div class="sdsync-toasts" aria-live="polite" aria-relevant="additions"><div v-for="toastItem in toasts" :key="toastItem.id" :class="['sdsync-toast', { 'is-error': toastItem.error }]" :role="toastItem.error ? 'alert' : 'status'"><strong>{{ toastItem.title }}</strong><span>{{ toastItem.message }}</span></div></div>
        <div v-if="confirmation.visible" class="sdsync-modal-backdrop" role="presentation" @click.self="settleConfirmation(false)">
          <div ref="confirmationDialog" class="sdsync-modal" role="dialog" aria-modal="true" aria-labelledby="sdsync-confirm-title" aria-describedby="sdsync-confirm-message" tabindex="-1">
            <p class="sdsync-eyebrow">Confirm action</p>
            <h2 id="sdsync-confirm-title">{{ confirmation.title }}</h2>
            <p id="sdsync-confirm-message">{{ confirmation.message }}</p>
            <div class="sdsync-action-row">
              <v-button ref="confirmationCancel" suffix="cancel" display="icon-text" aria-label="Cancel confirmation" @click="settleConfirmation(false)"><template #icon><action-icon name="close" /></template>Cancel</v-button>
              <v-button ref="confirmationAccept" suffix="red" display="icon-text" aria-label="Confirm action" @click="settleConfirmation(true)"><template #icon><action-icon name="confirm" /></template>{{ confirmation.button }}</v-button>
            </div>
          </div>
        </div>
      </div>
    </v-app-window>
  </v-app-instance>
</template>

<script>
import { ActionIcon } from "./ActionIcon";
import { installControlLayout } from "./controlLayout";
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
import SecurityPanel from "./SecurityPanel.vue";

const SETTINGS_KEY = "sdsync.ui.settings.v1";
const APP_CLASS = "SYNO.SDS.App.SynologyDriveSync.Instance";
const HELP_APPLICATION = "SYNO.SDS.HelpBrowser.Application";
const HELP_CONTENT = Object.freeze({
  overview: "overview.html",
  profiles: "profiles.html",
  routines: "routines.html",
  health: "health.html",
  activity: "activity.html",
  notifications: "notifications.html",
  security: "security.html",
  settings: "settings.html",
  about: "about.html"
});
const ABOUT_METADATA = Object.freeze({
  project: "synology-drive-sync",
  author: "Mariana",
  authorUrl: "https://github.com/supermarsx",
  maintainer: "supermarsx",
  maintainerUrl: "https://github.com/supermarsx/synology-drive-sync",
  repository: "https://github.com/supermarsx/synology-drive-sync",
  license: "MIT",
  licenseUrl: "https://github.com/supermarsx/synology-drive-sync/blob/main/LICENSE",
  coreVersion: "0.1.0",
  uiVersion: "1.0.0",
  apiSchema: SNAPSHOT_SCHEMA,
  releasesUrl: "https://github.com/supermarsx/synology-drive-sync/releases",
  releaseSelectorUrl: "https://supermarsx.github.io/synology-drive-sync/release-selector.html"
});
const ABOUT_RUST_DEPENDENCIES = Object.freeze([
  { name: "clap", pin: "4.6.5", scope: "All platforms", url: "https://crates.io/crates/clap" },
  { name: "clap_complete", pin: "4.6.8", scope: "All platforms", url: "https://crates.io/crates/clap_complete" },
  { name: "clap_mangen", pin: "0.3.0", scope: "All platforms", url: "https://crates.io/crates/clap_mangen" },
  { name: "ctrlc", pin: "3.5.2", scope: "All platforms", url: "https://crates.io/crates/ctrlc" },
  { name: "ignore", pin: "0.4.31", scope: "All platforms", url: "https://crates.io/crates/ignore" },
  { name: "keyring-core", pin: "1.0.0", scope: "All platforms", url: "https://crates.io/crates/keyring-core" },
  { name: "md-5", pin: "0.11.0", scope: "All platforms", url: "https://crates.io/crates/md-5" },
  { name: "hmac", pin: "0.12.1", scope: "All platforms", url: "https://crates.io/crates/hmac" },
  { name: "reqwest", pin: "0.13.4", scope: "All platforms", url: "https://crates.io/crates/reqwest" },
  { name: "rpassword", pin: "7.5.4", scope: "All platforms", url: "https://crates.io/crates/rpassword" },
  { name: "serde", pin: "1.0.229", scope: "All platforms", url: "https://crates.io/crates/serde" },
  { name: "serde_json", pin: "1.0.151", scope: "All platforms", url: "https://crates.io/crates/serde_json" },
  { name: "sha2", pin: "0.10.9", scope: "All platforms", url: "https://crates.io/crates/sha2" },
  { name: "subtle", pin: "2.6.1", scope: "All platforms", url: "https://crates.io/crates/subtle" },
  { name: "thiserror", pin: "2.0.19", scope: "All platforms", url: "https://crates.io/crates/thiserror" },
  { name: "toml", pin: "0.9.12+spec-1.1.0", scope: "All platforms", url: "https://crates.io/crates/toml" },
  { name: "totp-rs", pin: "5.7.2", scope: "All platforms", url: "https://crates.io/crates/totp-rs" },
  { name: "zeroize", pin: "1.9.0", scope: "All platforms", url: "https://crates.io/crates/zeroize" },
  { name: "windows-native-keyring-store", pin: "1.1.0", scope: "Windows", url: "https://crates.io/crates/windows-native-keyring-store" },
  { name: "apple-native-keyring-store", pin: "1.0.1", scope: "macOS", url: "https://crates.io/crates/apple-native-keyring-store" },
  { name: "libc", pin: "0.2.189", scope: "Linux", url: "https://crates.io/crates/libc" },
  { name: "zbus-secret-service-keyring-store", pin: "1.0.0", scope: "Linux", url: "https://crates.io/crates/zbus-secret-service-keyring-store" }
]);
const ABOUT_UI_DEPENDENCIES = Object.freeze([
  { name: "@babel/core", pin: "7.18.6", scope: "devDependency", url: "https://www.npmjs.com/package/@babel/core" },
  { name: "@babel/preset-env", pin: "7.18.6", scope: "devDependency", url: "https://www.npmjs.com/package/@babel/preset-env" },
  { name: "babel-loader", pin: "8.0.6", scope: "devDependency", url: "https://www.npmjs.com/package/babel-loader" },
  { name: "css-loader", pin: "3.5.3", scope: "devDependency", url: "https://www.npmjs.com/package/css-loader" },
  { name: "mini-css-extract-plugin", pin: "0.12.0", scope: "devDependency", url: "https://www.npmjs.com/package/mini-css-extract-plugin" },
  { name: "vue", pin: "2.7.14", scope: "devDependency", url: "https://www.npmjs.com/package/vue" },
  { name: "vue-loader", pin: "15.10.1", scope: "devDependency", url: "https://www.npmjs.com/package/vue-loader" },
  { name: "vue-template-compiler", pin: "2.7.14", scope: "devDependency", url: "https://www.npmjs.com/package/vue-template-compiler" },
  { name: "webpack", pin: "5.91.0", scope: "devDependency", url: "https://www.npmjs.com/package/webpack" },
  { name: "webpack-cli", pin: "5.1.4", scope: "devDependency", url: "https://www.npmjs.com/package/webpack-cli" },
  { name: "pnpm", pin: "pnpm@8.15.9", scope: "packageManager", url: "https://pnpm.io/" }
]);
const CONTROL_HELP = Object.freeze({
  "profile-filter": "Filter the local catalog by profile name.",
  "profile-name": "Stable profile identifier; existing names cannot be changed.",
  "profile-source": "Absolute NAS folder that supplies files for this one-way sync.",
  "profile-url": "HTTPS origin of the destination NAS File Station API.",
  "profile-username": "Destination DSM account used only by this profile.",
  "profile-remote": "Logical File Station destination path, not a local mount path.",
  "profile-compare": "Evidence used to decide whether a destination file needs upload.",
  "profile-jobs": "Parallel upload workers; accepted range is 1 through 16.",
  "profile-http": "Permit unencrypted HTTP only for an explicitly controlled LAN.",
  "profile-delete": "Allow deletion only when both the saved profile and a run approve it.",
  "profile-max-delete": "Hard per-profile ceiling that stops excessive destination deletion.",
  "profile-excludes": "Enter one package-relative exclusion pattern per line.",
  "profile-empty-source": "Disable the empty-source guard only when an empty source is intentional.",
  "profile-retries": "Retry count for transient upload failures; accepted range is 0 through 5.",
  "profile-timeout": "Maximum time allowed for one upload request.",
  "profile-connect-timeout": "Maximum time allowed to establish the destination connection.",
  "profile-rate": "Upload bandwidth ceiling in bytes per second; zero means unlimited.",
  "profile-ca": "Absolute NAS path to a trusted PEM certificate bundle.",
  "profile-invalid-certs": "Bypass TLS certificate validation; this exposes credentials to interception.",
  "profile-invalid-confirm": "Required acknowledgement before saving the unsafe TLS override.",
  "profile-verbosity": "Choose the amount of operational detail written to logs.",
  "profile-quiet": "Suppress the terminal sink while retaining durable package logs.",
  "profile-log-level": "Minimum severity retained by package logging.",
  "profile-log-url": "HTTPS endpoint that receives bounded structured log events.",
  "profile-log-mode": "Choose whether remote-log delivery failure can fail a sync.",
  "secret-log-mode": "Keep, replace, or clear the package-protected collector token.",
  "secret-log-value": "Replacement token; it is never returned to this window.",
  "secret-password-mode": "Keep, replace, or clear the package-protected DSM password.",
  "secret-password-value": "Replacement DSM password; it is never returned to this window.",
  "secret-totp-mode": "Keep, replace, or clear the package-protected TOTP seed.",
  "secret-totp-value": "Replacement TOTP material; it is never returned to this window.",
  "profile-default": "Select this profile when a command omits an explicit scope.",
  "routine-profile": "Profile whose automatic policy is being edited.",
  "routine-enabled": "Allow the package controller to start this routine automatically.",
  "routine-action": "Choose a real sync or a read-only plan for scheduled execution.",
  "routine-mode": "Run on an interval, inside a daily window, or after observed changes.",
  "routine-interval": "Seconds between interval executions; accepted range starts at 60.",
  "routine-window-start": "Local DSM time at which the daily execution window opens.",
  "routine-window-end": "Local DSM time at which the daily execution window closes.",
  "routine-debounce": "Quiet period after an observed change before a realtime run starts.",
  "routine-poll": "Fallback observation cadence when a native realtime hook is unavailable.",
  "routine-retries": "Additional routine attempts after a failed execution.",
  "routine-backoff": "Seconds to wait between routine retry attempts.",
  "routine-dependencies": "Require selected profile routines to finish before this routine starts.",
  "routine-weekdays": "Weekdays on which this routine may execute.",
  "routine-delete": "Permit this routine to use the profile's separately approved deletion policy.",
  "routine-max-delete": "Additional routine-level ceiling for destination deletions.",
  "doctor-scope": "Run diagnostics for every profile or one selected profile.",
  "doctor-write": "Create, verify, and remove one disposable destination probe.",
  "doctor-write-confirm": "Confirm that the selected destination is non-critical and cleanup is approved.",
  "activity-search": "Search the rendered event text, metadata, or an exact client request ID.",
  "activity-category": "Show structured activity from one audited subsystem or from every category.",
  "activity-level": "Show activity at one exact recorded severity or at every severity.",
  "log-source": "Limit the live view to DSM API, audit, controller, scheduler, sync, or all package logs.",
  "log-lines": "Maximum number of recent bounded log lines to display.",
  "alerts-enabled": "Allow fixed, non-secret package events to reach the DSM desktop.",
  "alerts-success": "Send a DSM desktop alert after a successful sync.",
  "alerts-failure": "Send a DSM desktop alert after the configured failure threshold.",
  "alerts-threshold": "Consecutive failures required before the package sends an alert.",
  "alerts-cooldown": "Minimum seconds between repeated failure alerts.",
  "session-notify": "Use browser notifications only while this AppWindow session is open.",
  "session-audible": "Play a short local cue for newly observed failures.",
  "settings-theme": "Use the dark ember theme, follow DSM system preference, or select light mode.",
  "settings-status-refresh": "Cadence for authenticated package status refreshes while visible.",
  "settings-log-refresh": "Cadence for live log refreshes while the Activity page is visible."
});

const ControlHelp = {
  name: "ControlHelp",
  components: { ActionIcon },
  props: { helpKey: { type: String, required: true } },
  computed: {
    helpId() { return `sdsync-help-${this.helpKey}`; },
    text() { return CONTROL_HELP[this.helpKey] || "See Synology Drive Sync in DSM Help."; }
  },
  template: `<span class="sdsync-field-tip"><button type="button" class="sdsync-field-tip-trigger" aria-label="Show field help" :aria-describedby="helpId" @keydown.esc="$event.currentTarget.blur()"><action-icon name="help" :size="14" /></button><span :id="helpId" class="sdsync-field-tip-content" role="tooltip">{{ text }}</span></span>`
};

function defaults() {
  return { theme: "dark", status_refresh: 5000, log_refresh: 5000, desktop_notifications: false, audible: false };
}

function settingsFromStoredValue(storedValue) {
  const fallback = defaults();
  try {
    const parsed = JSON.parse(storedValue || "null");
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

function loadSettings() {
  try {
    return settingsFromStoredValue(window.localStorage.getItem(SETTINGS_KEY));
  } catch (_error) {
    return defaults();
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

const SECURITY_BOOLEAN_FIELDS = Object.freeze([
  "require_https", "allow_interface_changes", "allow_profile_changes", "allow_secret_changes",
  "allow_routine_changes", "allow_notification_changes", "allow_operational_actions",
  "allow_http_targets", "allow_empty_source", "allow_invalid_tls", "allow_destructive_sync",
  "allow_doctor_write_test", "allow_remote_logging"
]);
const SECURITY_LOG_CATEGORIES = Object.freeze([
  "audit", "bridge", "authentication", "security", "configuration", "secrets",
  "routines", "operations", "notifications", "sync", "controller", "scheduler"
]);
const SECURITY_LOG_LEVELS = Object.freeze(["off", "trace", "debug", "info", "warn", "error"]);
const CLIENT_REQUEST_ID_PATTERN = /^[0-9a-f]{32}$/;
const JOB_ID_PATTERN = /^[0-9a-f]{48}$/;
const ACTIVITY_MESSAGE_LIMIT = 2048;
const ACTIVITY_FIELD_LIMIT = 128;
const MUTATION_MESSAGE_LIMIT = 4096;

function defaultSecurityPolicy() {
  return {
    policy_version: null,
    require_https: false,
    allow_interface_changes: true,
    allow_profile_changes: true,
    allow_secret_changes: true,
    allow_routine_changes: true,
    allow_notification_changes: true,
    allow_operational_actions: true,
    allow_http_targets: true,
    allow_empty_source: true,
    allow_invalid_tls: true,
    allow_destructive_sync: true,
    allow_doctor_write_test: true,
    allow_remote_logging: true,
    csrf_lifetime_seconds: 300,
    result_retention_seconds: 3600,
    max_outstanding_jobs: 256,
    log_levels: {
      audit: "info", bridge: "info", authentication: "warn", security: "warn",
      configuration: "info", secrets: "info", routines: "info", operations: "info",
      notifications: "warn", sync: "info", controller: "info", scheduler: "info"
    }
  };
}

function normalizedSecurityPolicy(source) {
  const fallback = defaultSecurityPolicy();
  if (!source || typeof source !== "object" || Array.isArray(source)) return fallback;
  const normalized = Object.assign({}, fallback);
  if (Number.isSafeInteger(source.policy_version) && source.policy_version > 0) {
    normalized.policy_version = source.policy_version;
  }
  SECURITY_BOOLEAN_FIELDS.forEach((field) => {
    if (!Object.prototype.hasOwnProperty.call(source, field)) return;
    if (typeof source[field] === "boolean") normalized[field] = source[field];
    else normalized[field] = field === "require_https";
  });
  for (const [field, minimum, maximum] of [
    ["csrf_lifetime_seconds", 60, 900],
    ["result_retention_seconds", 300, 86400],
    ["max_outstanding_jobs", 1, 256]
  ]) {
    const value = Number(source[field]);
    if (Number.isInteger(value) && value >= minimum && value <= maximum) normalized[field] = value;
  }
  const levels = source.log_levels && typeof source.log_levels === "object" && !Array.isArray(source.log_levels)
    ? source.log_levels
    : {};
  normalized.log_levels = Object.assign({}, fallback.log_levels);
  SECURITY_LOG_CATEGORIES.forEach((category) => {
    if (SECURITY_LOG_LEVELS.includes(levels[category])) normalized.log_levels[category] = levels[category];
  });
  return normalized;
}

function validatedClientRequestId(value) {
  return typeof value === "string" && CLIENT_REQUEST_ID_PATTERN.test(value) ? value : "";
}

function validatedJobId(value) {
  return typeof value === "string" && JOB_ID_PATTERN.test(value) ? value : "";
}

function normalizedActivityEvent(event) {
  if (!event || typeof event !== "object" || Array.isArray(event)) return null;
  const field = (value, fallback) => boundedText(value, fallback).slice(0, ACTIVITY_FIELD_LIMIT);
  return {
    epoch: numberOr(event.epoch, 0),
    code: field(event.code, "unknown.event"),
    profile: field(event.profile, "none"),
    state: field(event.state, "unknown"),
    category: field(event.category, "operations"),
    level: field(event.level, "info"),
    message: boundedText(event.message, "").slice(0, ACTIVITY_MESSAGE_LIMIT),
    client_request_id: validatedClientRequestId(event.client_request_id)
  };
}

function options(entries) {
  return entries.map(([value, label]) => ({ value, label }));
}

export default {
  name: "SynologyDriveSyncApp",
  components: { ActionIcon, ControlHelp, SecurityPanel },
  data() {
    const settings = loadSettings();
    return {
      routes: [
        { id: "overview", title: "Overview", icon: "overview" }, { id: "profiles", title: "Profiles", icon: "profiles" },
        { id: "routines", title: "Routines", icon: "routines" }, { id: "health", title: "Health / Doctor", icon: "health" },
        { id: "activity", title: "Activity / Logs", icon: "activity" }, { id: "notifications", title: "Notifications", icon: "notifications" },
        { id: "security", title: "Security", icon: "security" },
        { id: "settings", title: "Settings", icon: "settings" },
        { id: "about", title: "About", icon: "about" }
      ],
      route: "overview", auth: { signal: undefined }, csrfToken: "", snapshot: null,
      connected: false, connectionLabel: "Connecting to package…", freshness: "Waiting for status",
      bridgeIssue: { title: "", message: "" },
      snapshotTimer: 0, logTimer: 0, snapshotLoading: false, logsLoading: false, operationBusy: false,
      settings, profileFilter: "", profileEditorOpen: false, selectedProfile: "", profileForm: emptyProfile(),
      secretModes: { password: "keep", totp: "keep", remote_log_token: "keep" },
      secretValues: { password: "", totp: "", remote_log_token: "" },
      routineTabs: [
        { id: "configured-profiles", label: "Configured profiles" },
        { id: "package-controller", label: "Package controller" }
      ],
      routineTab: "configured-profiles",
      routineForm: emptyRoutine(), doctorForm: { scope: "all", write_test: false, write_confirm: false },
      alertForm: { enabled: false, on_success: false, on_failure: true, failure_threshold: 1, cooldown_seconds: 3600 },
      notificationTabs: [
        { id: "package-alerts", label: "Package alerts" },
        { id: "session-preferences", label: "Session preferences" }
      ],
      notificationTab: "package-alerts",
      securityForm: defaultSecurityPolicy(), securityDirty: false,
      notificationForm: { desktop_notifications: settings.desktop_notifications, audible: settings.audible },
      aboutMetadata: ABOUT_METADATA,
      aboutRustDependencies: ABOUT_RUST_DEPENDENCIES,
      aboutUiDependencies: ABOUT_UI_DEPENDENCIES,
      diagnostic: { title: "Not run in this session", output: "No diagnostic output yet." },
      logsPaused: false, logSource: "all", logLines: 200, logState: "Waiting for logs", logOutput: "No log data yet.", activityEvents: [], activitySearch: "", activityCategory: "all", activityLevel: "all",
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
    securityPolicy() { return normalizedSecurityPolicy(this.snapshot && this.snapshot.security_policy); },
    installedPackageVersion() { return boundedText(this.snapshot && this.snapshot.package && this.snapshot.package.version, "Not reported by package API"); },
    canMutate() { return this.capabilities.mutations === true && Boolean(this.csrfToken); },
    canChangeInterface() { return this.canMutate && this.securityPolicy.allow_interface_changes !== false; },
    canChangeProfiles() { return this.canMutate && this.securityPolicy.allow_profile_changes !== false; },
    canManageSecrets() { return this.canChangeProfiles && this.capabilities.secrets === true && this.securityPolicy.allow_secret_changes !== false; },
    canAllowHttp() { return this.canChangeProfiles && this.securityPolicy.allow_http_targets !== false; },
    canAllowEmptySource() { return this.canChangeProfiles && this.securityPolicy.allow_empty_source !== false; },
    canAllowInvalidTls() { return this.canChangeProfiles && this.securityPolicy.allow_invalid_tls !== false; },
    canAllowDestructive() { return this.canMutate && this.securityPolicy.allow_destructive_sync !== false; },
    canAllowRemoteLogging() { return this.canChangeProfiles && this.securityPolicy.allow_remote_logging !== false; },
    canEditHttpException() { return this.canChangeProfiles && (this.securityPolicy.allow_http_targets !== false || this.profileForm.allow_http === true); },
    canEditEmptySourceException() { return this.canChangeProfiles && (this.securityPolicy.allow_empty_source !== false || this.profileForm.allow_empty_source === true); },
    canEditInvalidTlsException() { return this.canChangeProfiles && (this.securityPolicy.allow_invalid_tls !== false || this.profileForm.danger_invalid_certs === true); },
    canEditProfileDeletion() { return this.canChangeProfiles && (this.securityPolicy.allow_destructive_sync !== false || this.profileForm.delete === true); },
    canEditRoutineDeletion() { return this.canChangeRoutines && (this.securityPolicy.allow_destructive_sync !== false || this.routineForm.allow_delete === true); },
    canEditRemoteLogging() { return this.canChangeProfiles && (this.securityPolicy.allow_remote_logging !== false || Boolean(this.profileForm.remote_log_url)); },
    canChangeRoutines() { return this.canMutate && this.securityPolicy.allow_routine_changes !== false; },
    canChangeNotifications() { return this.canMutate && this.securityPolicy.allow_notification_changes !== false; },
    canRunOperations() { return this.canMutate && this.securityPolicy.allow_operational_actions !== false; },
    canRunDoctorWrite() { return this.securityPolicy.allow_doctor_write_test !== false; },
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
    reversedActivity() {
      const query = boundedText(this.activitySearch, "").trim().toLowerCase().slice(0, ACTIVITY_FIELD_LIMIT);
      return this.activityEvents.map(normalizedActivityEvent).filter((event) => {
        if (!event) return false;
        const category = event.category.toLowerCase();
        const level = event.level.toLowerCase();
        const searchable = [
          event.code, event.profile, event.state, event.category, event.level,
          event.message, event.client_request_id
        ].join("\n").toLowerCase();
        return (this.activityCategory === "all" || category === this.activityCategory)
          && (this.activityLevel === "all" || level === this.activityLevel)
          && (!query || searchable.includes(query));
      }).reverse();
    },
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
    logSourceOptions() { return options([["all", "All logs"], ["api", "DSM API"], ["audit", "Audit"], ["controller", "Controller"], ["scheduler", "Scheduler"], ["sync", "Sync"]]); },
    activityCategoryOptions() { return options([["all", "All categories"], ["audit", "Audit"], ["bridge", "Bridge"], ["authentication", "Authentication"], ["security", "Security"], ["configuration", "Configuration"], ["secrets", "Secrets"], ["routines", "Routines"], ["operations", "Operations"], ["notifications", "Notifications"], ["sync", "Sync"], ["controller", "Controller"], ["scheduler", "Scheduler"]]); },
    activityLevelOptions() { return options([["all", "All levels"], ...["trace", "debug", "info", "warn", "error"].map((level) => [level, level])]); },
    logLineOptions() { return options([[100, "100 lines"], [200, "200 lines"], [500, "500 lines"], [1000, "1000 lines"]]); },
    themeOptions() { return options([["dark", "Hellfire dark"], ["system", "Follow system"], ["light", "Ash light"]]); },
    statusRefreshOptions() { return options([[3000, "Every 3 seconds"], [5000, "Every 5 seconds"], [10000, "Every 10 seconds"], [30000, "Every 30 seconds"]]); },
    logRefreshOptions() { return options([[5000, "Every 5 seconds"], [10000, "Every 10 seconds"], [30000, "Every 30 seconds"]]); }
  },
  async mounted() {
    this.controlLayoutCleanup = installControlLayout(this.$el);
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
    let csrfReady = false;
    try {
      await this.refreshCsrf();
      csrfReady = true;
    } catch (error) {
      if (this.disposed) return;
      this.connected = false;
      this.csrfToken = "";
      this.bridgeIssue = this.describeBridgeError(error, "authentication");
      this.connectionLabel = this.bridgeIssue.title;
      this.toast(this.bridgeIssue.title, this.bridgeIssue.message, true);
    }
    if (this.disposed) return;
    if (csrfReady) await this.refreshSnapshot(false);
    else this.scheduleSnapshot();
  },
  beforeDestroy() {
    this.disposed = true;
    if (this.abortController) this.abortController.abort();
    this.stopTimers();
    this.toastTimers.forEach((timer) => window.clearTimeout(timer));
    this.toastTimers = [];
    if (this.visibilityHandler) document.removeEventListener("visibilitychange", this.visibilityHandler);
    if (this.mediaQuery && this.mediaQuery.removeEventListener && this.mediaHandler) this.mediaQuery.removeEventListener("change", this.mediaHandler);
    if (this.controlLayoutCleanup) this.controlLayoutCleanup();
    this.removeConfirmationKeyHandler();
    if (this.confirmation.resolve) this.confirmation.resolve(false);
    this.confirmationPriorFocus = null;
    this.clearSecrets();
    this.csrfToken = "";
    this.auth = { signal: undefined };
  },
  methods: {
    formatBytes, formatDate, formatDuration,
    describeBridgeError(error, phase = "status") {
      const status = Number(error && error.status) || 0;
      const code = String((error && error.code) || "").toLowerCase();
      const message = String((error && error.message) || "").toLowerCase();
      const stage = boundedText(error && error.stage, "").trim().slice(0, 128);
      const issue = (title, detail) => ({
        title,
        message: stage ? `${detail} Failure stage: ${stage}.` : detail
      });
      if (status === 401) {
        return issue("DSM session expired", "Sign in to DSM again, then reopen this app from the DSM desktop.");
      }
      if (status === 403) {
        return issue("DSM access denied", "Use a DSM administrator account and, if HTTPS is required by policy, reopen this app over HTTPS.");
      }
      if (status === 400) {
        return issue("DSM request metadata rejected", "Install or repair the latest complete package release, then reopen this app from the DSM desktop.");
      }
      if (status === 404) {
        return issue("Package UI route unavailable", "DSM did not reach this package's native API. Repair or reinstall the same package release, then reopen the app.");
      }
      if (status === 503) {
        return issue("Package service unavailable", "Restart Synology Drive Sync and inspect its API log if the package bridge does not recover.");
      }
      if (status === 0 && (code.includes("unauthorized") || code.includes("authentication") || message.includes("redirect"))) {
        return issue("DSM session expired", "Sign in to DSM again, then reopen this app from the DSM desktop.");
      }
      if (status === 0 && code.includes("forbidden")) {
        return issue("DSM access denied", "Use a DSM administrator account and, if HTTPS is required by policy, reopen this app over HTTPS.");
      }
      if ((status === 0 || (status >= 200 && status < 300)) && (code === "non_json_response" || code === "malformed_json")) {
        return issue("Package UI route unavailable", "DSM did not reach this package's native API. Repair or reinstall the same package release, then reopen the app.");
      }
      if (status === 0 && code.includes("unavailable")) {
        return issue("Package service unavailable", "Restart Synology Drive Sync and inspect its API log if the package bridge does not recover.");
      }
      if (message.includes("unsupported dsm api schema") || code === "invalid_document") {
        return issue("UI and package versions differ", "Repair or reinstall one complete release so the AppWindow and package API use the same schema.");
      }
      if (message.includes("cancel")) {
        return issue("DSM request cancelled", "The window stopped the request. Retry while this AppWindow remains open.");
      }
      const fallback = phase === "authentication"
        ? "DSM authentication could not be completed. Reopen this app from the DSM desktop."
        : "The package endpoint could not be reached. Confirm the package is running, then retry.";
      return issue("Package bridge unavailable", fallback);
    },
    openDsmHelp() {
      /* global SYNO */
      const launch = typeof SYNO !== "undefined" && SYNO.SDS && SYNO.SDS.AppLaunch;
      if (typeof launch !== "function") {
        this.toast("DSM Help unavailable", "Open DSM Help and select Synology Drive Sync.", true);
        return;
      }
      try {
        launch(HELP_APPLICATION, { app: APP_CLASS, content: HELP_CONTENT[this.route] || HELP_CONTENT.overview }, false);
      } catch (_error) {
        this.toast("DSM Help unavailable", "Open DSM Help and select Synology Drive Sync.", true);
      }
    },
    navigate(route) { if (!this.routes.some((item) => item.id === route)) return; if (this.route === "profiles" && route !== "profiles") this.closeProfile(); this.route = route; if (route === "activity") this.refreshLogs(); else window.clearTimeout(this.logTimer); },
    moveSubtab(stateKey, tabs, event) {
      if (!event || !Array.isArray(tabs) || !tabs.length) return;
      const current = Math.max(0, tabs.findIndex((tab) => tab.id === this[stateKey]));
      let next = current;
      if (event.key === "ArrowRight") next = (current + 1) % tabs.length;
      else if (event.key === "ArrowLeft") next = (current - 1 + tabs.length) % tabs.length;
      else if (event.key === "Home") next = 0;
      else if (event.key === "End") next = tabs.length - 1;
      else return;
      const tablist = event.currentTarget;
      event.preventDefault();
      this[stateKey] = tabs[next].id;
      this.$nextTick(() => {
        const buttons = tablist && tablist.querySelectorAll ? tablist.querySelectorAll('[role="tab"]') : [];
        if (buttons[next] && typeof buttons[next].focus === "function") buttons[next].focus();
      });
    },
    pillClass(state) { const value = String(state || "unknown").toLowerCase(); return ["sdsync-pill", { failed: ["failed", "error", "untrusted", "denied"].includes(value), neutral: ["disabled", "stopped", "unknown", "default", "unsupported", "unavailable"].includes(value) }]; },
    healthClass(value) { return value === true ? "sdsync-health-ok" : (value === false ? "sdsync-health-bad" : "sdsync-health-unknown"); },
    booleanEvidence(value) { return value === true ? "Yes" : (value === false ? "No" : "Unavailable"); },
    reportMutationError(error, failedTitle, unknownTitle, fallback) {
      const unknown = Boolean(error && error.outcomeUnknown === true);
      const observed = boundedText(error && error.message, fallback).slice(0, MUTATION_MESSAGE_LIMIT / 2);
      const requestId = error && error.trustedRequestId === true
        ? validatedClientRequestId(error.requestId)
        : "";
      const jobId = error && error.trustedJobId === true
        ? validatedJobId(error.jobId)
        : "";
      const correlation = [
        requestId ? `Client request ID: ${requestId}.` : "",
        jobId ? `Queued job ID: ${jobId}.` : ""
      ].filter(Boolean).join(" ");
      const withCorrelation = (detail, defaultMessage) => boundedText(
        correlation ? `${correlation} ${detail}` : detail,
        defaultMessage
      ).slice(0, MUTATION_MESSAGE_LIMIT);
      const csrfRejected = Boolean(error && error.preAcceptance === true && error.csrfRejected === true);
      if (csrfRejected) {
        this.csrfToken = "";
        this.bridgeIssue = {
          title: "Mutation token rejected",
          message: "Select Retry to request a fresh DSM mutation token, review the current state, and then submit again. The rejected POST was not accepted and was not retried."
        };
        this.connectionLabel = this.bridgeIssue.title;
        const message = withCorrelation(`${observed} ${this.bridgeIssue.message}`, this.bridgeIssue.message);
        this.toast(failedTitle, message, true);
        return { unknown: false, csrfRejected: true, message, requestId, jobId };
      }
      const message = unknown
        ? withCorrelation(
          error && error.acceptanceUnknown === true
            ? `${observed} Preserve the client request ID and inspect Activity and Logs before taking any further action.`
            : `${observed} The request was already queued; do not retry it or create a duplicate. Inspect Activity and Logs for the eventual outcome.`,
          "The operation outcome is unknown. Do not retry it; inspect Activity and Logs."
        )
        : withCorrelation(observed, fallback);
      this.toast(unknown ? unknownTitle : failedTitle, message, !unknown);
      return { unknown, message, requestId, jobId };
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
        this.bridgeIssue = { title: "", message: "" };
        this.connectionLabel = this.canMutate ? "Authenticated package bridge" : "Package status · read-only";
        this.freshness = `Updated ${new Intl.DateTimeFormat(undefined, { timeStyle: "medium" }).format(new Date())}`;
        this.hydrateAlerts();
        this.hydrateSecurityPolicy();
        this.maybeNotifyFailure();
        if (manual) this.toast("Status refreshed", "The latest package snapshot is displayed.");
      } catch (error) {
        if (this.disposed) return;
        this.csrfToken = "";
        this.connected = false;
        this.bridgeIssue = this.describeBridgeError(error, "status");
        this.connectionLabel = this.bridgeIssue.title;
        this.freshness = this.snapshot ? "Stale · last successful snapshot retained" : "Status unavailable";
        if (manual) this.toast(this.bridgeIssue.title, this.bridgeIssue.message, true);
      } finally { this.snapshotLoading = false; if (!this.disposed) this.scheduleSnapshot(); }
    },
    hydrateAlerts() { const alerts = this.snapshot && this.snapshot.alerts; if (!alerts || typeof alerts !== "object") return; this.alertForm = { enabled: alerts.enabled === true, on_success: alerts.on_success === true, on_failure: alerts.on_failure !== false, failure_threshold: numberOr(alerts.failure_threshold, 1), cooldown_seconds: numberOr(alerts.cooldown_seconds, 3600) }; },
    hydrateSecurityPolicy(force = false) {
      if (this.securityDirty && !force) return;
      const policy = normalizedSecurityPolicy(this.snapshot && this.snapshot.security_policy);
      this.securityForm = Object.assign({}, policy, { log_levels: Object.assign({}, policy.log_levels) });
      this.securityDirty = false;
    },
    updateSecurityForm(value) {
      if (!value || typeof value !== "object" || Array.isArray(value)) return;
      this.securityForm = Object.assign({}, value, { log_levels: Object.assign({}, value.log_levels || {}) });
      this.securityDirty = true;
    },
    securityPayload() {
      const payload = {};
      SECURITY_BOOLEAN_FIELDS.forEach((field) => { payload[field] = this.securityForm[field]; });
      payload.csrf_lifetime_seconds = Number(this.securityForm.csrf_lifetime_seconds);
      payload.result_retention_seconds = Number(this.securityForm.result_retention_seconds);
      payload.max_outstanding_jobs = Number(this.securityForm.max_outstanding_jobs);
      const levels = this.securityForm.log_levels && typeof this.securityForm.log_levels === "object"
        ? this.securityForm.log_levels
        : {};
      SECURITY_LOG_CATEGORIES.forEach((category) => { payload[`${category}_log_level`] = levels[category]; });
      return payload;
    },
    validateSecurityPayload(payload) {
      if (SECURITY_BOOLEAN_FIELDS.some((field) => typeof payload[field] !== "boolean")) {
        return "Every security permission and risk ceiling must be explicitly enabled or disabled.";
      }
      if (!this.between(payload.csrf_lifetime_seconds, 60, 900)) return "CSRF lifetime must be between 60 and 900 seconds.";
      if (!this.between(payload.result_retention_seconds, 300, 86400)) return "Result retention must be between 300 and 86400 seconds.";
      if (!this.between(payload.max_outstanding_jobs, 1, 256)) return "Maximum outstanding jobs must be between 1 and 256.";
      if (SECURITY_LOG_CATEGORIES.some((category) => !SECURITY_LOG_LEVELS.includes(payload[`${category}_log_level`]))) {
        return "Every log category must use off, trace, debug, info, warn, or error.";
      }
      return "";
    },
    securityRelaxed(payload) {
      const current = this.securityPolicy;
      if (current.require_https === true && payload.require_https === false) return true;
      return SECURITY_BOOLEAN_FIELDS.some((field) => field !== "require_https" && current[field] === false && payload[field] === true);
    },
    async saveSecurityPolicy(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (!this.canMutate || !this.securityDirty || this.operationBusy) return;
      const payload = this.securityPayload();
      const error = this.validateSecurityPayload(payload);
      if (error) return this.toast("Security policy not saved", error, true);
      if (this.securityRelaxed(payload) && !await this.confirmAction(
        "Relax security restrictions?",
        "One or more administrator permissions or risk ceilings will become less restrictive. Review the complete policy before continuing.",
        "Save relaxed policy"
      )) return;
      this.operationBusy = true;
      try {
        await apiPost(this.auth, this.csrfToken, ACTIONS.securityPolicy, payload);
        if (this.disposed) return;
        this.securityDirty = false;
        this.csrfToken = "";
        try {
          await this.refreshCsrf();
        } catch (_csrfError) {
          if (this.disposed) return;
          this.connected = false;
          this.bridgeIssue = {
            title: "Mutation token refresh required",
            message: "The security policy was saved, but DSM did not issue a replacement mutation token. Select Retry to request one; do not repeat the save."
          };
          this.connectionLabel = this.bridgeIssue.title;
          this.freshness = this.snapshot ? "Stale · last successful snapshot retained" : "Status unavailable";
          this.toast("Security policy saved · refresh required", this.bridgeIssue.message, true);
          return;
        }
        if (this.disposed) return;
        this.toast("Security policy saved", "The package validated, persisted, enforced, and audited the complete policy.");
        await this.refreshSnapshot(false);
        if (!this.disposed) this.hydrateSecurityPolicy(true);
      } catch (caught) {
        if (this.disposed) return;
        const report = this.reportMutationError(caught, "Security policy not saved", "Security policy outcome unknown", "The package rejected the security policy.");
        if (report.unknown) {
          this.csrfToken = "";
          this.securityDirty = false;
          await this.refreshSnapshot(false);
          if (!this.disposed && this.connected) this.hydrateSecurityPolicy(true);
        }
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    openProfile(name) {
      if (this.operationBusy) return;
      if (!name && !this.canChangeProfiles) return;
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
      if (payload.allow_http && !this.canAllowHttp) return "The security policy does not permit HTTP destinations.";
      if (payload.allow_empty_source && !this.canAllowEmptySource) return "The security policy does not permit empty-source exceptions.";
      if (payload.danger_accept_invalid_certs && !this.canAllowInvalidTls) return "The security policy does not permit invalid TLS certificates.";
      if (payload.delete && !this.canAllowDestructive) return "The security policy does not permit deletion-capable profiles.";
      if (payload.remote_log_url && !this.canAllowRemoteLogging) return "The security policy does not permit remote logging.";
      if (secrets.length && !this.canManageSecrets) return "The security policy does not permit protected-secret changes.";
      if (secrets.some((item) => item.kind === "remote-log-token") && !this.canAllowRemoteLogging) return "The security policy does not permit remote-log token changes.";
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
      if (!this.canChangeProfiles || this.operationBusy) return;
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
      if (!this.canChangeProfiles || !this.selectedProfile || this.operationBusy) return;
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
    selectRoutine(profile) { if (this.operationBusy) return; this.routineForm.profile = profile; this.loadRoutine(profile); this.routineTab = "package-controller"; },
    routinePayload() { return { profile: this.routineForm.profile, enabled: this.routineForm.enabled === true, action: this.routineForm.action, mode: this.routineForm.mode, interval_seconds: this.integer(this.routineForm.interval_seconds, 3600), weekdays: this.routineForm.weekdays.map(Number), time_window_start: this.routineForm.time_window_start, time_window_end: this.routineForm.time_window_end, debounce_seconds: this.integer(this.routineForm.debounce_seconds, 30), poll_seconds: this.integer(this.routineForm.poll_seconds, 30), retry_count: this.integer(this.routineForm.retry_count, 2), retry_backoff_seconds: this.integer(this.routineForm.retry_backoff_seconds, 60), allow_delete: this.routineForm.allow_delete === true, max_total_delete: this.integer(this.routineForm.max_total_delete, 100), depends_on: this.routineForm.depends_on.map(String) }; },
    async saveRoutine(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (!this.canChangeRoutines || !this.routineForm.profile || this.operationBusy) return;
      const payload = this.routinePayload();
      if (!payload.weekdays.length) return this.toast("Routine not saved", "Select at least one active weekday.", true);
      if (payload.allow_delete && !this.canAllowDestructive) return this.toast("Routine not saved", "The security policy does not permit deletion-capable routines.", true);
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
      if (!this.canChangeRoutines || !profile || !this.selectedRoutine || this.operationBusy) return;
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
      if (!this.canChangeNotifications || this.operationBusy) return;
      const payload = { enabled: this.alertForm.enabled === true, on_success: this.alertForm.on_success === true, on_failure: this.alertForm.on_failure === true, failure_threshold: this.integer(this.alertForm.failure_threshold, 1), cooldown_seconds: this.integer(this.alertForm.cooldown_seconds, 3600) };
      if (!this.between(payload.failure_threshold, 1, 100) || !this.between(payload.cooldown_seconds, 60, 2592000)) return this.toast("Alert policy not saved", "Failure threshold or cooldown is outside the supported range.", true);
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
      if (!this.canRunOperations || this.operationBusy || this.disposed) return;
      if (payload && payload.allow_delete === true && !this.canAllowDestructive) return;
      if (kind === "doctor" && payload && payload.write_test === true && (!this.canRunDoctorWrite || !this.hasCapability("write_test"))) return;
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
    async quickRun() { if (!this.canRunOperations || this.operationBusy) return; if (await this.confirmAction("Run all configured profiles?", "This starts a real one-way sync. Remote deletion stays disabled for this quick action.", "Run all")) return this.executeOperation("run", { scope: "all", write_test: null, allow_delete: false, max_total_delete: 0 }); },
    async runDoctor(event) { if (event && event.preventDefault) event.preventDefault(); if (!this.canRunOperations || this.operationBusy) return; if (this.doctorForm.write_test && (!this.canRunDoctorWrite || !this.hasCapability("write_test"))) return this.toast("Doctor write test blocked", "The package capability or security policy does not permit disposable destination probes.", true); if (this.doctorForm.write_test && !this.doctorForm.write_confirm) return this.toast("Write-test confirmation required", "Approve the disposable probe and cleanup before running.", true); if (this.doctorForm.write_test && !await this.confirmAction("Run the disposable target probe?", "The doctor briefly creates, verifies, and removes a unique probe in the selected destination.", "Run write test")) return; this.diagnostic = { title: "Doctor running", output: "Waiting for the package controller…" }; return this.executeOperation("doctor", { scope: this.doctorForm.scope, write_test: this.doctorForm.write_test, allow_delete: null, max_total_delete: null }); },
    logsFrom(model) { if (Array.isArray(model.logs)) return model.logs.map((entry) => { if (typeof entry === "string") return entry; if (entry && typeof entry === "object") { if (Array.isArray(entry.lines)) return entry.lines.map((line) => `[${boundedText(entry.source, "log")}] ${boundedText(line, "")}`).join("\n"); return `${entry.timestamp ? `[${entry.timestamp}] ` : ""}${entry.source ? `[${entry.source}] ` : ""}${boundedText(entry.message, "")}`; } return ""; }).join("\n"); return boundedText(model.text || model.output, "No log data yet."); },
    async refreshLogs() { if (this.disposed || this.logsLoading || this.logsPaused || document.hidden || this.route !== "activity") return; this.logsLoading = true; try { const lines = Math.min(1000, Math.max(1, Number(this.logLines) || 200)); const [logs, activity] = await Promise.all([apiGet(this.auth, "logs", { lines, source: this.logSource }), apiGet(this.auth, "activity", { lines })]); if (this.disposed) return; this.logOutput = this.logsFrom(logs).slice(0, MAX_RESPONSE_BYTES); this.activityEvents = arrayOf(activity.events); this.logState = `Live · ${lines} line limit`; } catch (_error) { if (!this.disposed) this.logState = "Logs unavailable"; } finally { this.logsLoading = false; if (!this.disposed) this.scheduleLogs(); } },
    toggleLogs() { this.logsPaused = !this.logsPaused; this.logState = this.logsPaused ? "Paused" : "Resuming"; if (!this.logsPaused) this.refreshLogs(); else window.clearTimeout(this.logTimer); },
    clearLogView() { this.logOutput = "View cleared. The package log was not deleted."; },
    async saveNotificationPreferences(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (!this.canChangeNotifications || this.operationBusy) return;
      this.operationBusy = true;
      let transaction = null;
      try {
        if (this.notificationForm.desktop_notifications && window.Notification && Notification.permission === "default") {
          const permission = await Notification.requestPermission();
          if (permission !== "granted") this.notificationForm.desktop_notifications = false;
        }
        transaction = this.captureSettingsTransaction();
        if (!transaction) return;
        const next = Object.assign({}, transaction.settings, {
          desktop_notifications: this.notificationForm.desktop_notifications === true,
          audible: this.notificationForm.audible === true
        });
        if (!this.persistSettings(next)) {
          this.applySettingsState(transaction.settings);
          return;
        }
        this.applySettingsState(next);
        await apiPost(this.auth, this.csrfToken, ACTIONS.clientEvent, { event: "session-notifications" });
        if (this.disposed) return;
        this.toast("Session preferences saved", "These non-secret browser preferences were audited and stored locally.");
      } catch (error) {
        if (!this.disposed) {
          const rejected = this.preferenceAuditWasRejected(error);
          const restored = rejected && transaction ? this.restoreSettingsTransaction(transaction) : false;
          this.reportMutationError(
            error,
            rejected && restored ? "Session preferences not saved" : (rejected ? "Session preference rollback incomplete" : "Session preferences stored · audit failed"),
            "Session preferences stored · audit outcome unknown",
            rejected && restored
              ? "The package rejected the audit event and the prior browser preferences were restored."
              : "The browser preferences remain stored locally, but the package audit did not complete."
          );
        }
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    async saveInterfaceSettings(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (!this.canChangeInterface || this.operationBusy) return;
      const candidate = {
        theme: this.settings.theme,
        status_refresh: Number(this.settings.status_refresh),
        log_refresh: Number(this.settings.log_refresh)
      };
      if (!["dark", "light", "system"].includes(candidate.theme)
        || ![3000, 5000, 10000, 30000].includes(candidate.status_refresh)
        || ![5000, 10000, 30000].includes(candidate.log_refresh)) {
        this.toast("Interface settings not saved", "Choose a supported theme and refresh cadence.", true);
        return;
      }
      this.operationBusy = true;
      let transaction = null;
      try {
        transaction = this.captureSettingsTransaction();
        if (!transaction) return;
        const next = Object.assign({}, transaction.settings, candidate);
        if (!this.persistSettings(next)) {
          this.applySettingsState(transaction.settings);
          this.scheduleSnapshot();
          this.scheduleLogs();
          return;
        }
        this.applySettingsState(next);
        this.scheduleSnapshot();
        this.scheduleLogs();
        await apiPost(this.auth, this.csrfToken, ACTIONS.clientEvent, { event: "interface-settings" });
        if (this.disposed) return;
        this.toast("Interface settings saved", "Theme and refresh cadence were audited and stored locally.");
      } catch (error) {
        if (!this.disposed) {
          const rejected = this.preferenceAuditWasRejected(error);
          const restored = rejected && transaction ? this.restoreSettingsTransaction(transaction) : false;
          if (rejected) {
            this.scheduleSnapshot();
            this.scheduleLogs();
          }
          this.reportMutationError(
            error,
            rejected && restored ? "Interface settings not saved" : (rejected ? "Interface setting rollback incomplete" : "Interface settings stored · audit failed"),
            "Interface settings stored · audit outcome unknown",
            rejected && restored
              ? "The package rejected the audit event and the prior interface settings were restored."
              : "The interface settings remain stored locally, but the package audit did not complete."
          );
        }
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    persistSettings(settings = this.settings) {
      try {
        window.localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings));
        return true;
      } catch (_error) {
        this.toast("Preferences not persisted", "Browser storage is unavailable for this DSM session.", true);
        return false;
      }
    },
    captureSettingsTransaction() {
      try {
        const raw = window.localStorage.getItem(SETTINGS_KEY);
        return { raw, settings: settingsFromStoredValue(raw) };
      } catch (_error) {
        this.toast("Preferences not persisted", "Browser storage is unavailable for this DSM session; no package audit was submitted.", true);
        return null;
      }
    },
    applySettingsState(settings) {
      this.settings = Object.assign({}, settings);
      this.notificationForm = Object.assign({}, this.notificationForm, {
        desktop_notifications: settings.desktop_notifications === true,
        audible: settings.audible === true
      });
    },
    restoreSettingsTransaction(transaction) {
      let restored = true;
      try {
        if (transaction.raw === null) window.localStorage.removeItem(SETTINGS_KEY);
        else window.localStorage.setItem(SETTINGS_KEY, transaction.raw);
      } catch (_error) {
        restored = false;
        this.toast("Preference rollback incomplete", "The prior browser settings could not be restored in local storage. Review this AppWindow before continuing.", true);
      }
      this.applySettingsState(transaction.settings);
      return restored;
    },
    preferenceAuditWasRejected(error) {
      return Boolean(error && error.preAcceptance === true && error.trustedRejection === true);
    },
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
