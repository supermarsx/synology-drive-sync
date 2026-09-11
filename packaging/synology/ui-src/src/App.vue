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
              <span :class="['sdsync-autosave-state', 'is-' + autosavePhase]" aria-live="polite"><action-icon name="save" :size="13" />{{ autosaveMessage }}</span>
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
                :tooltip="snapshotRefreshTooltip"
                :disabled="snapshotLoading || snapshotRefreshBlocked"
                @click="refreshSnapshot(true)"
                :aria-busy="snapshotLoading ? 'true' : 'false'"
              ><template #icon><action-icon :class="{ 'sdsync-is-spinning': snapshotLoading }" name="refresh" /></template>Refresh</v-button>
            </div>
            <transition name="sdsync-live-operation">
              <div v-if="profileLiveOperation" class="sdsync-live-operation" aria-hidden="true">
                <span class="sdsync-live-operation-indicator"><action-icon class="sdsync-is-spinning" name="refresh" size="22" /></span>
                <span class="sdsync-live-operation-copy">
                  <strong>{{ profileLiveOperation.title }}</strong>
                  <span v-if="profileLiveOperation.stage" class="sdsync-live-operation-stage">{{ profileLiveOperation.stage }}</span>
                  <span>{{ profileLiveOperation.message }}</span>
                  <span v-if="profileLiveOperation.warning" class="sdsync-live-operation-warning">{{ profileLiveOperation.warning }}</span>
                </span>
              </div>
            </transition>
          </header>

          <div v-if="!canMutate" class="sdsync-banner" role="status">
            <div>
              <strong>{{ bridgeIssue.title || 'Read-only mode' }}</strong>
              <span>{{ bridgeIssue.message || 'Live status remains available, but changes stay disabled until the authenticated DSM bridge is ready.' }}</span>
              <v-button type="border" display="icon-text" :tooltip="snapshotRefreshBlocked ? 'Close the profile editor before retrying package status' : 'Retry DSM authentication and reload package status'" :disabled="snapshotLoading || snapshotRefreshBlocked" :aria-busy="snapshotLoading ? 'true' : 'false'" @click="refreshSnapshot(true)"><template #icon><action-icon :class="{ 'sdsync-is-spinning': snapshotLoading }" name="refresh" /></template>Retry</v-button>
            </div>
          </div>

          <div v-if="incidentOutcomeUnresolved" class="sdsync-banner sdsync-mutation-barrier" role="alert" aria-live="assertive">
            <div>
              <strong>Scoped outcome needs reconciliation</strong>
              <span>{{ incidentGuidance }}</span>
              <span class="sdsync-barrier-gates">
                <small><strong>Blocked:</strong> {{ incidentScopeAvailability.blocked }}</small>
                <small v-if="incidentScopeAvailability.available"><strong>Available:</strong> {{ incidentScopeAvailability.available }}</small>
              </span>
              <span v-if="incidentProbeAccount" class="sdsync-barrier-progress" role="status" aria-live="polite">{{ incidentProbeAccount }}</span>
              <v-button
                v-if="incidentProbeTargets.length"
                type="border"
                display="icon-text"
                tooltip="Read the package queue and Activity for the exact preserved request ID; no new request is submitted"
                :disabled="!canCheckIncidentOutcomes"
                :aria-busy="incidentProbe.active ? 'true' : 'false'"
                @click="checkIncidentOutcomes(true)"
              ><template #icon><action-icon :class="{ 'sdsync-is-spinning': incidentProbe.active }" name="refresh" /></template>{{ incidentProbe.active ? 'Checking…' : 'Check again now' }}</v-button>
              <v-button
                v-if="profileReconciliationIncident && hasCapability('request_reconciliation')"
                type="border"
                display="icon-text"
                tooltip="Resolve this exact client request ID through the authenticated private queue; no new mutation is submitted"
                :disabled="!canReconcileProfileIncident"
                :aria-busy="profileReconciliationState === 'checking' ? 'true' : 'false'"
                @click="reconcileProfileIncident"
              ><template #icon><action-icon :class="{ 'sdsync-is-spinning': profileReconciliationState === 'checking' }" name="refresh" /></template>{{ profileReconciliationState === 'checking' ? 'Reconciling…' : 'Reconcile profile request' }}</v-button>
              <v-button
                v-else-if="connectionReconciliationIncident && hasCapability('request_reconciliation')"
                type="border"
                display="icon-text"
                tooltip="Resolve this exact authentication or File Station request through the authenticated private queue; no new request is submitted"
                :disabled="!canReconcileConnectionIncident"
                :aria-busy="profileReconciliationState === 'checking' ? 'true' : 'false'"
                @click="reconcileConnectionIncident"
              ><template #icon><action-icon :class="{ 'sdsync-is-spinning': profileReconciliationState === 'checking' }" name="refresh" /></template>{{ profileReconciliationState === 'checking' ? 'Reconciling…' : 'Reconcile connection request' }}</v-button>
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
                <v-button suffix="grey" display="icon-text" :tooltip="operationMutationGuidance || 'Preview every configured profile without changing destination files'" :disabled="!canRunOperations || !profiles.length || operationBusy" @click="quickPlan"><template #icon><action-icon name="plan" /></template>Plan all profiles</v-button>
                <v-button suffix="main" display="icon-text" :tooltip="operationMutationGuidance || 'Start a real sync for every configured profile with deletion disabled'" :disabled="!canRunOperations || !profiles.length || operationBusy" @click="quickRun"><template #icon><action-icon name="run" /></template>Run all profiles</v-button>
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
            <div v-if="!profileEditorOpen" class="sdsync-page-actions">
              <v-button suffix="main" display="icon-text" tooltip="Create a validated source-to-File-Station profile" :disabled="!canChangeProfiles || operationBusy" @click="openProfile('')"><template #icon><action-icon name="add" /></template>New profile</v-button>
            </div>
            <div :class="['sdsync-profiles-layout', profileEditorOpen ? 'is-editor-only' : 'is-catalog-only']">
              <transition name="sdsync-page-swap" mode="out-in">
              <div v-if="!profileEditorOpen" key="profile-catalog" class="sdsync-panel sdsync-profile-catalog">
                <div class="sdsync-profile-filters" role="search" aria-label="Profile catalog filters">
                  <div class="sdsync-profile-filter-field"><span class="sdsync-profile-filter-label">Search <control-help help-key="profile-filter" /></span><v-input class="sdsync-input-control" v-model="profileFilter" clearable maxlength="128" placeholder="Name, target, account, or path" aria-label="Search profiles" aria-describedby="sdsync-help-profile-filter" /></div>
                  <div class="sdsync-profile-filter-field"><span class="sdsync-profile-filter-label">Show <control-help help-key="profile-filter-status" /></span><v-single-select class="sdsync-select-control" v-model="profileFilterStatus" :options="profileFilterStatusOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-label="Filter profiles by readiness" aria-describedby="sdsync-help-profile-filter-status"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></div>
                  <div class="sdsync-profile-filter-summary" aria-live="polite"><span>{{ profileFilterSummary }}</span><v-button v-if="profileFiltersActive" type="styleless" display="icon-text" tooltip="Clear every profile catalog filter" @click="clearProfileFilters"><template #icon><action-icon name="close" /></template>Clear</v-button></div>
                </div>
                <p v-if="!filteredProfiles.length" class="sdsync-empty">{{ profiles.length ? 'No matching profiles.' : 'No configured profiles.' }}</p>
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

              <v-form v-else key="profile-editor" v-model="profileForm" class="sdsync-panel sdsync-editor sdsync-profile-editor" direction="vertical" @submit="saveProfile">
                <div class="sdsync-panel-heading">
                  <div><p class="sdsync-eyebrow">Profile editor</p><h3>{{ selectedProfile ? 'Edit ' + selectedProfile : 'New profile' }}</h3></div>
                  <v-button type="border" display="icon-text" :tooltip="profileOutcomeUnresolved || connectionOutcomeUnresolved ? profileDraftRecoveryGuidance : 'Close the editor and clear unsubmitted secret fields'" :disabled="profileSaveState === 'saving' || profileConnectionState === 'testing' || profileReconciliationState === 'checking' || profileOutcomeUnresolved || connectionOutcomeUnresolved" @click="closeProfile"><template #icon><action-icon name="close" /></template>Close</v-button>
                </div>
                <div class="sdsync-form-grid">
                  <v-form-item class="sdsync-form-item" label="Name" prop="name"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-name" /></template><v-input class="sdsync-input-control" v-model.trim="profileForm.name" :readonly="Boolean(selectedProfile)" maxlength="64" placeholder="office_nas" aria-describedby="sdsync-help-profile-name" :disabled="!canChangeProfiles" /></v-form-item>
                  <v-form-item class="sdsync-form-item" label="Local source" prop="source"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-source" /></template><div class="sdsync-path-control"><v-input class="sdsync-input-control" v-model.trim="profileForm.source" maxlength="4096" placeholder="/volume1/Source" aria-describedby="sdsync-help-profile-source" :disabled="!canChangeProfiles" /><v-button type="border" display="icon-text" html-type="button" tooltip="Browse only NAS folders the package identity can read and traverse" :disabled="!canChangeProfiles || pathBrowser.loading" @click="openLocalSourceBrowser"><template #icon><action-icon name="folder" /></template>Browse NAS</v-button></div></v-form-item>
                  <aside class="sdsync-permission-callout span-2" role="note" aria-label="DSM shared-folder permission required">
                    <span class="sdsync-permission-callout-icon" aria-hidden="true"><span>!</span></span>
                    <span class="sdsync-permission-callout-copy"><strong>Shared-folder access must be granted first</strong>In DSM, open <code>Control Panel → Shared Folder → Permissions → System internal user</code>, then grant list, traverse, and read access to the exact package identity DSM displays. DSM can collision-rename that identity; the package cannot grant itself access.</span>
                  </aside>
                  <v-form-item class="sdsync-form-item span-2" label="File Station URL" prop="url"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-url" /></template><v-input class="sdsync-input-control" v-model.trim="profileForm.url" maxlength="2048" placeholder="https://files.example.com" aria-describedby="sdsync-help-profile-url" :disabled="!canChangeProfiles" /></v-form-item>
                  <v-form-item class="sdsync-form-item" label="DSM username" prop="username"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-username" /></template><v-input class="sdsync-input-control" v-model.trim="profileForm.username" maxlength="256" autocomplete="username" aria-describedby="sdsync-help-profile-username" :disabled="!canChangeProfiles" /></v-form-item>
                  <v-form-item class="sdsync-form-item" label="Remote logical path" prop="remote"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-remote" /></template><div class="sdsync-path-control"><v-input class="sdsync-input-control" v-model.trim="profileForm.remote" maxlength="247" placeholder="/home/Drive/NAS Backup" aria-describedby="sdsync-help-profile-remote" :disabled="!canChangeProfiles" /><v-button type="border" display="icon-text" html-type="button" :tooltip="profileConnectionActionGuidance || (connectionTestReady ? 'Browse directories returned by the authenticated File Station account' : 'Test authentication successfully before browsing File Station')" :disabled="!canChangeProfiles || profileConnectionBlocked || !connectionTestReady || pathBrowser.loading" @click="openRemotePathBrowser"><template #icon><action-icon name="folder" /></template>{{ profileConnectionBlocked ? 'Browse locked' : (connectionOutcomeUnresolved ? 'Retry target browse' : 'Browse target') }}</v-button></div></v-form-item>
                  <v-form-item class="sdsync-form-item" label="Comparison"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-compare" /></template><v-single-select class="sdsync-select-control" v-model="profileForm.compare" :options="compareOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-profile-compare" :disabled="!canChangeProfiles"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                  <v-form-item class="sdsync-form-item" label="Concurrent uploads"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-jobs" /></template><v-input class="sdsync-input-control" v-model="profileForm.jobs" number-only aria-describedby="sdsync-help-profile-jobs" :disabled="!canChangeProfiles" /></v-form-item>
                  <div class="sdsync-toggle-row span-2"><span class="sdsync-toggle-label">Allow plain HTTP for controlled LAN testing <control-help help-key="profile-http" /></span><v-checkbox class="sdsync-checkbox-control" v-model="profileForm.allow_http" aria-label="Allow plain HTTP for controlled LAN testing" aria-describedby="sdsync-help-profile-http" :disabled="!canEditHttpException" /></div>
                </div>

                <fieldset class="sdsync-danger-fieldset">
                  <legend>Deletion guard</legend>
                  <div class="sdsync-toggle-row"><span class="sdsync-toggle-label">Mirror remote deletions after profile and run-level approval <control-help help-key="profile-delete" /></span><v-checkbox class="sdsync-checkbox-control" v-model="profileForm.delete" aria-label="Mirror remote deletions after profile and run-level approval" aria-describedby="sdsync-help-profile-delete" :disabled="!canEditProfileDeletion" /></div>
                  <v-form-item class="sdsync-form-item" label="Maximum deletions per run"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-max-delete" /></template><v-input class="sdsync-input-control" v-model="profileForm.max_delete" number-only aria-describedby="sdsync-help-profile-max-delete" :disabled="!canChangeProfiles" /></v-form-item>
                </fieldset>

                <details class="sdsync-advanced">
                  <summary><strong><action-icon name="settings" />&nbsp;Advanced profile controls</strong><span>Network, retry, output, and remote observability policy</span></summary>
                  <div class="sdsync-form-grid">
                    <v-form-item class="sdsync-form-item span-2" label="Excludes"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-excludes" /></template><v-input class="sdsync-input-control" v-model="profileForm.excludes" type="textarea" :autosize="{ minRows: 3, maxRows: 7 }" placeholder="@eaDir/&#10;**/@eaDir/&#10;#recycle/" aria-describedby="sdsync-help-profile-excludes" :disabled="!canChangeProfiles" /></v-form-item>
                    <div class="sdsync-toggle-row span-2"><span class="sdsync-toggle-label">Allow an empty source (disables the empty-source deletion guard) <control-help help-key="profile-empty-source" /></span><v-checkbox class="sdsync-checkbox-control" v-model="profileForm.allow_empty_source" aria-label="Allow an empty source" aria-describedby="sdsync-help-profile-empty-source" :disabled="!canEditEmptySourceException" /></div>
                    <v-form-item class="sdsync-form-item" label="Retries"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-retries" /></template><v-input class="sdsync-input-control" v-model="profileForm.retries" number-only aria-describedby="sdsync-help-profile-retries" :disabled="!canChangeProfiles" /></v-form-item>
                    <v-form-item class="sdsync-form-item" label="Upload timeout (seconds)"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-timeout" /></template><v-input class="sdsync-input-control" v-model="profileForm.timeout" number-only aria-describedby="sdsync-help-profile-timeout" :disabled="!canChangeProfiles" /></v-form-item>
                    <v-form-item class="sdsync-form-item" label="Connect timeout (seconds)"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-connect-timeout" /></template><v-input class="sdsync-input-control" v-model="profileForm.connect_timeout" number-only aria-describedby="sdsync-help-profile-connect-timeout" :disabled="!canChangeProfiles" /></v-form-item>
                    <v-form-item class="sdsync-form-item" label="Maximum rate (bytes/s)"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-rate" /></template><v-input class="sdsync-input-control" v-model="profileForm.max_rate" number-only aria-describedby="sdsync-help-profile-rate" :disabled="!canChangeProfiles" /></v-form-item>
                    <v-form-item class="sdsync-form-item span-2" label="CA certificate path"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-ca" /></template><v-input class="sdsync-input-control" v-model.trim="profileForm.ca_certificate" maxlength="4096" placeholder="/volume1/certificates/ca.pem" aria-describedby="sdsync-help-profile-ca" :disabled="!canChangeProfiles" /></v-form-item>
                    <div class="sdsync-toggle-row span-2"><span class="sdsync-toggle-label">Accept invalid TLS certificates (unsafe) <control-help help-key="profile-invalid-certs" /></span><v-checkbox class="sdsync-checkbox-control" v-model="profileForm.danger_invalid_certs" aria-label="Accept invalid TLS certificates" aria-describedby="sdsync-help-profile-invalid-certs" :disabled="!canEditInvalidTlsException" /></div>
                    <div v-if="profileForm.danger_invalid_certs" class="sdsync-toggle-row is-danger span-2"><span class="sdsync-toggle-label">I accept the interception risk <control-help help-key="profile-invalid-confirm" /></span><v-checkbox class="sdsync-checkbox-control" v-model="profileForm.danger_invalid_confirm" aria-label="Accept the interception risk" aria-describedby="sdsync-help-profile-invalid-confirm" :disabled="!canEditInvalidTlsException" /></div>
                    <v-form-item class="sdsync-form-item" label="Verbosity"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-verbosity" /></template><v-single-select class="sdsync-select-control" v-model="profileForm.verbosity" :options="verbosityOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-profile-verbosity" :disabled="!canChangeProfiles"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                    <div class="sdsync-toggle-row"><span class="sdsync-toggle-label">Quiet terminal sink; durable logs remain active <control-help help-key="profile-quiet" /></span><v-checkbox class="sdsync-checkbox-control" v-model="profileForm.quiet" aria-label="Use quiet terminal output" aria-describedby="sdsync-help-profile-quiet" :disabled="!canChangeProfiles" /></div>
                    <v-form-item class="sdsync-form-item" label="Log level"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-log-level" /></template><v-single-select class="sdsync-select-control" v-model="profileForm.log_level" :options="logLevelOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-profile-log-level" :disabled="!canChangeProfiles"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                    <v-form-item class="sdsync-form-item" label="Log format"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-log-format" /></template><v-single-select class="sdsync-select-control" v-model="profileForm.log_format" :options="logFormatOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-profile-log-format" :disabled="!canChangeProfiles"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                    <v-form-item class="sdsync-form-item" label="Progress"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-progress" /></template><v-single-select class="sdsync-select-control" v-model="profileForm.progress" :options="progressOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-profile-progress" :disabled="!canChangeProfiles"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                    <v-form-item class="sdsync-form-item" label="Output"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-output" /></template><v-single-select class="sdsync-select-control" v-model="profileForm.output" :options="outputOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-profile-output" :disabled="!canChangeProfiles"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                    <v-form-item class="sdsync-form-item span-2" label="Log file" textonly><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-log-file" /></template><span class="sdsync-readonly-value">{{ profileLogFile }}</span></v-form-item>
                    <v-form-item class="sdsync-form-item span-2" label="Remote log URL"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-log-url" /></template><v-input class="sdsync-input-control" v-model.trim="profileForm.remote_log_url" maxlength="2048" placeholder="https://collector.example.com/ingest" aria-describedby="sdsync-help-profile-log-url" :disabled="!canEditRemoteLogging" /></v-form-item>
                    <v-form-item class="sdsync-form-item" label="Remote log mode"><template #label-after><control-help class="sdsync-form-label-help" help-key="profile-log-mode" /></template><v-single-select class="sdsync-select-control" v-model="profileForm.remote_log_mode" :options="remoteLogModeOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-profile-log-mode" :disabled="!canEditRemoteLogging"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                  </div>
                  <div class="sdsync-secret-editor">
                    <div class="sdsync-secret-summary"><strong>Remote log token</strong><span>{{ selectedProfileModel && selectedProfileModel.has_remote_log_token ? 'Stored · masked' : 'Not stored' }}</span></div>
                    <v-single-select class="sdsync-select-control sdsync-secret-mode" v-model="secretModes.remote_log_token" :options="secretModeOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-secret-log-mode" :disabled="!canManageSecrets"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select><control-help class="sdsync-secret-mode-help" help-key="secret-log-mode" />
                    <v-input class="sdsync-input-control sdsync-secret-value" v-if="secretModes.remote_log_token === 'replace'" v-model="secretValues.remote_log_token" type="password" maxlength="4096" autocomplete="new-password" placeholder="New token" aria-describedby="sdsync-help-secret-log-value" :disabled="!canReplaceRemoteLogToken" /><control-help class="sdsync-secret-value-help" v-if="secretModes.remote_log_token === 'replace'" help-key="secret-log-value" />
                  </div>
                </details>

                <fieldset class="sdsync-secret-fieldset">
                  <legend>Protected credentials</legend>
                  <div class="sdsync-secret-editor">
                    <div class="sdsync-secret-summary"><strong>Password</strong><span>{{ selectedProfileModel && selectedProfileModel.has_password ? 'Stored · masked' : 'Not stored' }}</span></div>
                    <v-single-select class="sdsync-select-control sdsync-secret-mode" v-model="secretModes.password" :options="secretModeOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-secret-password-mode" :disabled="!canManageSecrets"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select><control-help class="sdsync-secret-mode-help" help-key="secret-password-mode" />
                    <v-input class="sdsync-input-control sdsync-secret-value" v-if="secretModes.password === 'replace'" v-model="secretValues.password" type="password" maxlength="4096" autocomplete="new-password" placeholder="New password" aria-describedby="sdsync-help-secret-password-value" :disabled="!canManageSecrets" /><control-help class="sdsync-secret-value-help" v-if="secretModes.password === 'replace'" help-key="secret-password-value" />
                  </div>
                  <div class="sdsync-secret-editor">
                    <div class="sdsync-secret-summary"><strong>TOTP seed</strong><span>{{ selectedProfileModel && selectedProfileModel.has_totp ? 'Stored · masked' : 'Not stored' }}</span></div>
                    <v-single-select class="sdsync-select-control sdsync-secret-mode" v-model="secretModes.totp" :options="secretModeOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-secret-totp-mode" :disabled="!canManageSecrets"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select><control-help class="sdsync-secret-mode-help" help-key="secret-totp-mode" />
                    <v-input class="sdsync-input-control sdsync-secret-value" v-if="secretModes.totp === 'replace'" v-model="secretValues.totp" type="password" maxlength="4096" autocomplete="off" placeholder="Base32 seed or otpauth URI" aria-describedby="sdsync-help-secret-totp-value" :disabled="!canManageSecrets" /><control-help class="sdsync-secret-value-help" v-if="secretModes.totp === 'replace'" help-key="secret-totp-value" />
                  </div>
                  <div class="sdsync-connection-test span-2">
                    <v-button type="border" display="icon-text" html-type="button" :tooltip="profileConnectionActionGuidance || 'Authenticate with this draft without storing it, then close the temporary File Station session'" :disabled="!canTestProfileAuthentication" :aria-busy="profileConnectionState === 'testing' ? 'true' : 'false'" @click="testProfileAuthentication"><template #icon><action-icon :class="{ 'sdsync-is-spinning': profileConnectionState === 'testing' }" :name="profileConnectionState === 'testing' ? 'refresh' : 'doctor'" /></template>{{ profileConnectionState === 'testing' ? 'Testing authentication…' : (profileConnectionBlocked ? 'Authentication locked' : (connectionOutcomeUnresolved ? 'Reconciliation required' : 'Test authentication')) }}</v-button>
                    <span :class="['sdsync-connection-state', 'is-' + profileConnectionState]" role="status" aria-live="polite">{{ profileConnectionMessage }}</span>
                  </div>
                  <p class="sdsync-field-note">Secret values are sent only in the protected request body. They are never returned to this window.</p>
                  <div v-if="selectedProfile" class="sdsync-secret-actions">
                    <v-button suffix="main" display="icon-text" html-type="button" :tooltip="profileOutcomeUnresolved ? profileOutcomeGuidance : 'Apply only changed password, TOTP, and remote-log token operations without rewriting profile configuration'" :disabled="!canSubmitProfileSecrets" :aria-busy="profileSaveState === 'saving' ? 'true' : 'false'" @click="saveProfileSecrets"><template #icon><action-icon :class="{ 'sdsync-is-spinning': profileSaveState === 'saving' }" :name="profileSaveState === 'saving' ? 'refresh' : 'save'" /></template>{{ profileSaveState === 'saving' ? 'Saving changed secrets…' : (profileOutcomeUnresolved ? 'Secrets locked' : 'Save changed secrets') }}</v-button>
                  </div>
                </fieldset>

                <div class="sdsync-toggle-row"><span class="sdsync-toggle-label">Use as default profile <control-help help-key="profile-default" /></span><v-checkbox class="sdsync-checkbox-control" v-model="profileForm.make_default" aria-label="Use as default profile" aria-describedby="sdsync-help-profile-default" :disabled="!canChangeProfiles" /></div>
                <div class="sdsync-form-actions">
                  <v-button v-if="selectedProfile" suffix="red" display="icon-text" :tooltip="profileOutcomeUnresolved ? profileOutcomeGuidance : 'Remove package configuration and stored credentials, not synchronized files'" :disabled="!canRemoveProfile" @click="removeProfile"><template #icon><action-icon name="delete" /></template>Delete profile</v-button>
                  <span class="sdsync-field-note">Existing safe profile changes autosave after 1.3 seconds. Creation, secrets, and new risk approvals stay explicit.</span>
                  <v-button suffix="cancel" display="icon-text" :tooltip="profileOutcomeUnresolved || connectionOutcomeUnresolved ? profileDraftRecoveryGuidance : 'Discard unsaved editor values and clear secret fields'" :disabled="profileSaveState === 'saving' || profileConnectionState === 'testing' || profileReconciliationState === 'checking' || profileOutcomeUnresolved || connectionOutcomeUnresolved" @click="closeProfile"><template #icon><action-icon name="close" /></template>Cancel</v-button>
                  <v-button suffix="main" display="icon-text" html-type="submit" :tooltip="profileOutcomeUnresolved ? profileOutcomeGuidance : 'Validate and apply configuration immediately, then process explicit secret operations'" :disabled="!canSubmitProfile" :aria-busy="profileSaveState === 'saving' ? 'true' : 'false'"><template #icon><action-icon :class="{ 'sdsync-is-spinning': profileSaveState === 'saving' }" :name="profileSaveState === 'saving' ? 'refresh' : 'save'" /></template>{{ profileSaveButtonText }}</v-button>
                </div>
                <p v-if="profileSaveMessage" :class="['sdsync-save-state', 'is-' + profileSaveState]" role="status" aria-live="polite">{{ profileSaveMessage }}</p>
              </v-form>
              </transition>
            </div>
          </section>

          <section v-else-if="route === 'routines'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <div class="sdsync-page-actions">
              <v-button suffix="main" display="icon-text" tooltip="Create a per-profile automation routine" :disabled="!canChangeRoutines || !profiles.length || operationBusy" @click="openRoutine('')"><template #icon><action-icon name="add" /></template>New routine</v-button>
            </div>
            <div :class="['sdsync-routines-layout', routineEditorOpen ? 'is-editor-only' : 'is-catalog-only']">
              <transition name="sdsync-page-swap" mode="out-in">
              <article v-if="!routineEditorOpen" key="routine-catalog" class="sdsync-panel sdsync-routine-catalog">
                <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Configured routines</p><h3>Per-profile automation</h3></div><span class="sdsync-mini-badge">{{ routines.length }} total</span></div>
                <p v-if="!routines.length" class="sdsync-empty">No configured routines. Choose New routine to automate a profile.</p>
                <button v-for="routine in routines" :key="routine.profile" type="button" :class="['sdsync-routine-row', { 'is-selected': selectedRoutine && selectedRoutine.profile === routine.profile }]" :title="'Edit routine for ' + routine.profile" :disabled="operationBusy" @click="openRoutine(routine.profile)"><span><strong><action-icon name="edit" />&nbsp;{{ routine.profile }}</strong><small>{{ routine.mode || 'interval' }} · {{ routine.backend || 'fallback unreported' }} · {{ routine.state || (routine.enabled ? 'enabled' : 'disabled') }}</small></span><time>{{ routine.enabled ? formatDate(routine.next_run_epoch) : 'Disabled' }}</time></button>
              </article>
              <v-form v-else key="routine-editor" v-model="routineForm" class="sdsync-panel sdsync-editor sdsync-horizontal-form sdsync-routine-editor" direction="horizontal" @submit="saveRoutine">
                    <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Routine editor</p><h3>{{ selectedRoutine ? 'Edit ' + selectedRoutine.profile : 'New profile routine' }}</h3></div><v-button type="border" display="icon-text" tooltip="Close the routine editor and discard unsaved values" @click="closeRoutine"><template #icon><action-icon name="close" /></template>Close</v-button></div>
                    <v-form-item class="sdsync-form-item sdsync-inline-form-item" label="Profile" label-flex="0 0 150px" control-flex="1 1 auto"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-profile" /></template><v-single-select class="sdsync-select-control" v-model="routineForm.profile" :options="profileOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-routine-profile" :disabled="!canChangeRoutines || operationBusy" @input="loadRoutine"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                    <div class="sdsync-toggle-row"><span class="sdsync-toggle-label">Enable routine <control-help help-key="routine-enabled" /></span><v-checkbox class="sdsync-checkbox-control" v-model="routineForm.enabled" aria-label="Enable routine" aria-describedby="sdsync-help-routine-enabled" :disabled="!canChangeRoutines" /></div>
                    <div class="sdsync-form-grid compact sdsync-routine-fields">
                      <v-form-item class="sdsync-form-item sdsync-inline-form-item" label="Action" label-flex="0 0 150px" control-flex="1 1 auto"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-action" /></template><v-single-select class="sdsync-select-control" v-model="routineForm.action" :options="routineActionOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-routine-action" :disabled="!canChangeRoutines"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                      <v-form-item class="sdsync-form-item sdsync-inline-form-item" label="Mode" label-flex="0 0 150px" control-flex="1 1 auto"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-mode" /></template><v-single-select class="sdsync-select-control" v-model="routineForm.mode" :options="routineModeOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-routine-mode" :disabled="!canChangeRoutines"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                      <v-form-item v-if="routineForm.mode === 'interval'" class="sdsync-form-item sdsync-inline-form-item" label="Interval (seconds)" label-flex="0 0 150px" control-flex="1 1 auto"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-interval" /></template><v-input class="sdsync-input-control" v-model="routineForm.interval_seconds" number-only aria-describedby="sdsync-help-routine-interval" :disabled="!canChangeRoutines" /></v-form-item>
                      <v-form-item v-if="routineForm.mode === 'daily'" class="sdsync-form-item sdsync-inline-form-item" label="Window starts" label-flex="0 0 150px" control-flex="1 1 auto"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-window-start" /></template><input v-model="routineForm.time_window_start" class="sdsync-native-input" type="time" aria-label="Window starts" aria-describedby="sdsync-help-routine-window-start" :disabled="!canChangeRoutines"></v-form-item>
                      <v-form-item v-if="routineForm.mode === 'daily'" class="sdsync-form-item sdsync-inline-form-item" label="Window ends" label-flex="0 0 150px" control-flex="1 1 auto"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-window-end" /></template><input v-model="routineForm.time_window_end" class="sdsync-native-input" type="time" aria-label="Window ends" aria-describedby="sdsync-help-routine-window-end" :disabled="!canChangeRoutines"></v-form-item>
                      <v-form-item v-if="routineForm.mode === 'realtime'" class="sdsync-form-item sdsync-inline-form-item" label="Realtime debounce (seconds)" label-flex="0 0 150px" control-flex="1 1 auto"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-debounce" /></template><v-input class="sdsync-input-control" v-model="routineForm.debounce_seconds" number-only aria-describedby="sdsync-help-routine-debounce" :disabled="!canChangeRoutines" /></v-form-item>
                      <v-form-item v-if="routineForm.mode === 'realtime'" class="sdsync-form-item sdsync-inline-form-item" label="Fallback poll (seconds)" label-flex="0 0 150px" control-flex="1 1 auto"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-poll" /></template><v-input class="sdsync-input-control" v-model="routineForm.poll_seconds" number-only aria-describedby="sdsync-help-routine-poll" :disabled="!canChangeRoutines" /></v-form-item>
                      <v-form-item class="sdsync-form-item sdsync-inline-form-item" label="Retry attempts" label-flex="0 0 150px" control-flex="1 1 auto"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-retries" /></template><v-input class="sdsync-input-control" v-model="routineForm.retry_count" number-only aria-describedby="sdsync-help-routine-retries" :disabled="!canChangeRoutines" /></v-form-item>
                      <v-form-item class="sdsync-form-item sdsync-inline-form-item" label="Retry backoff (seconds)" label-flex="0 0 150px" control-flex="1 1 auto"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-backoff" /></template><v-input class="sdsync-input-control" v-model="routineForm.retry_backoff_seconds" number-only min="10" max="300" aria-describedby="sdsync-help-routine-backoff" :disabled="!canChangeRoutines" /></v-form-item>
                      <div class="sdsync-toggle-row"><span class="sdsync-toggle-label">Use exponential retry backoff (maximum 300 seconds) <control-help help-key="routine-exponential" /></span><v-checkbox class="sdsync-checkbox-control" v-model="routineForm.retry_exponential" aria-label="Use exponential retry backoff" aria-describedby="sdsync-help-routine-exponential" :disabled="!canChangeRoutines" /></div>
                      <v-form-item class="sdsync-form-item span-2" label="Wait for routines">
                        <template #label-after><control-help class="sdsync-form-label-help" help-key="routine-dependencies" /></template>
                        <select v-model="routineForm.depends_on" class="sdsync-native-input" multiple size="4" aria-label="Wait for routines" aria-describedby="sdsync-help-routine-dependencies" :disabled="!canChangeRoutines">
                          <option v-for="profile in dependencyProfiles" :key="profile.name" :value="profile.name">{{ profile.name }}</option>
                        </select>
                      </v-form-item>
                    </div>
                    <fieldset v-if="routineForm.mode === 'daily'" class="sdsync-weekday-fieldset" aria-describedby="sdsync-help-routine-weekdays" :disabled="!canChangeRoutines"><legend>Active weekdays <control-help help-key="routine-weekdays" /></legend><div class="sdsync-weekdays"><label v-for="day in weekdayOptions" :key="day.value"><input v-model="routineForm.weekdays" type="checkbox" :value="day.value" :disabled="!canChangeRoutines"><span>{{ day.label }}</span></label></div></fieldset>
                    <fieldset class="sdsync-danger-fieldset"><legend>Routine deletion guard</legend><div class="sdsync-toggle-row"><span class="sdsync-toggle-label">Permit profile deletion rules <control-help help-key="routine-delete" /></span><v-checkbox class="sdsync-checkbox-control" v-model="routineForm.allow_delete" aria-label="Permit profile deletion rules" aria-describedby="sdsync-help-routine-delete" :disabled="!canEditRoutineDeletion" /></div><v-form-item class="sdsync-form-item sdsync-inline-form-item" label="Routine deletion approval ceiling"><template #label-after><control-help class="sdsync-form-label-help" help-key="routine-max-delete" /></template><v-input class="sdsync-input-control" v-model="routineForm.max_total_delete" number-only aria-describedby="sdsync-help-routine-max-delete" :disabled="!canChangeRoutines" /></v-form-item></fieldset>
                    <div class="sdsync-form-actions"><v-button suffix="red" display="icon-text" :tooltip="routineMutationBlocked ? routineMutationGuidance : 'Remove this automation policy without deleting its profile'" :disabled="!canRemoveRoutine" @click="removeRoutine"><template #icon><action-icon name="delete" /></template>Remove routine</v-button><span class="sdsync-field-note">Existing safe routine changes autosave after 1.3 seconds. Creation and deletion approval stay explicit.</span><v-button suffix="cancel" display="icon-text" tooltip="Discard unsaved routine values" @click="closeRoutine"><template #icon><action-icon name="close" /></template>Cancel</v-button><v-button suffix="main" display="icon-text" html-type="submit" :tooltip="routineMutationBlocked ? routineMutationGuidance : 'Validate and apply this per-profile automation policy immediately'" :disabled="!canSubmitRoutine"><template #icon><action-icon name="save" /></template>{{ routineMutationBlocked ? 'Save locked' : 'Save now' }}</v-button></div>
              </v-form>
              </transition>
            </div>
          </section>

          <section v-else-if="route === 'sync'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <article class="sdsync-panel sdsync-rollup" aria-labelledby="sdsync-rollup-title">
              <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Stored totals</p><h3 id="sdsync-rollup-title">Sync summary</h3></div><div class="sdsync-evidence-heading-actions"><span class="sdsync-freshness">{{ statusRollupFreshness }}</span><v-button type="border" display="icon-text" aria-label="Re-read the stored totals" tooltip="Re-read what the last check recorded. This walks no folder and contacts no destination." :disabled="statusRollupLoading" :aria-busy="statusRollupLoading ? 'true' : 'false'" @click="refreshStatusRollup"><template #icon><action-icon :class="{ 'sdsync-is-spinning': statusRollupLoading }" name="refresh" /></template>Refresh</v-button></div></div>
              <p class="sdsync-field-note">Read back from what the last check recorded, so it opens straight away: no folder is walked and the destination is not contacted. Check one scope below when you need a live per-file comparison.</p>
              <p v-if="!statusRollup.loaded || (!statusRollup.total && !statusRollup.profiles.length)" class="sdsync-empty">{{ statusRollupMessage }}</p>
              <template v-else>
                <dl v-if="statusRollupCards.length" class="sdsync-sync-stats sdsync-rollup-stats" aria-label="Combined stored totals">
                  <div v-for="card in statusRollupCards" :key="card.id" :class="['sdsync-sync-stat', 'is-' + card.id]"><dt>{{ card.label }}</dt><dd>{{ card.value }}</dd><small>{{ card.detail }}</small></div>
                </dl>
                <p v-if="statusRollupTotalNote" class="sdsync-field-note">{{ statusRollupTotalNote }}</p>
                <div v-if="statusRollupRows.length" class="sdsync-table-wrap"><table><thead><tr><th>Profile</th><th>In sync</th><th>Pending upload</th><th>Needs attention</th><th>Observed</th></tr></thead><tbody><tr v-for="row in statusRollupRows" :key="row.profile"><td>{{ row.profile }}</td><td>{{ row.inSync }}</td><td>{{ row.pending }}</td><td>{{ row.attention }}</td><td :title="row.observedExact">{{ row.observed }}</td></tr></tbody></table></div>
                <p v-if="statusRollupNeverObservedNote" class="sdsync-field-note">{{ statusRollupNeverObservedNote }}</p>
              </template>
            </article>

            <v-form v-model="syncStatusForm" class="sdsync-panel sdsync-sync-query" @submit="checkSyncStatus">
              <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Per-file state</p><h3>Check one scope</h3></div><span class="sdsync-pill neutral">On demand</span></div>
              <p class="sdsync-field-note">This check caches nothing: both sides are compared again every time, so its answer is never stale &#8212; and never free. The summary above is the cached one. This page has no refresh timer for that reason.</p>
              <div class="sdsync-filter-list" aria-label="Sync status query">
                <div v-for="field in syncTextFields" :key="field.key" class="sdsync-filter-row"><span class="sdsync-filter-label">{{ field.label }}</span><div class="sdsync-filter-control"><v-input class="sdsync-input-control" :value="syncStatusForm[field.key]" clearable :maxlength="field.max" :placeholder="field.placeholder" :aria-label="field.label" :aria-describedby="'sdsync-help-' + field.help" :disabled="syncLocked" @input="setSyncField(field, $event)" /><control-help :help-key="field.help" /></div></div>
                <div v-for="field in syncSelectFields" :key="field.key" class="sdsync-filter-row"><span class="sdsync-filter-label">{{ field.label }}</span><div class="sdsync-filter-control"><v-single-select class="sdsync-select-control" :value="syncStatusForm[field.key]" :options="field.options" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" :aria-label="field.label" :aria-describedby="'sdsync-help-' + field.help" :disabled="syncLocked" @input="setSyncField(field, $event)"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select><control-help :help-key="field.help" /></div></div>
              </div>
              <div class="sdsync-toggle-row"><span class="sdsync-toggle-label">Also walk excluded entries <control-help help-key="sync-excluded" /></span><v-checkbox class="sdsync-checkbox-control" v-model="syncStatusForm.include_excluded" aria-label="Also walk excluded entries" aria-describedby="sdsync-help-sync-excluded" :disabled="syncLocked" /></div>
              <div class="sdsync-sync-actions">
                <v-button v-if="syncStatusFiltersActive" type="border" display="icon-text" html-type="button" tooltip="Reset to the default attention view" :disabled="syncStatusBusy" @click="clearSyncFilters"><template #icon><action-icon name="close" /></template>Clear filters</v-button>
                <v-button suffix="main" display="icon-text" html-type="submit" :tooltip="operationMutationGuidance || 'Compare both sides and load the first page'" :disabled="!syncStatusReady" :aria-busy="syncStatusBusy ? 'true' : 'false'"><template #icon><action-icon :class="{ 'sdsync-is-spinning': syncStatusBusy }" name="refresh" /></template>{{ syncStatusBusy ? 'Checking&#8230;' : 'Check status' }}</v-button>
              </div>
            </v-form>

            <article class="sdsync-panel sdsync-sync-results" aria-live="polite" aria-labelledby="sdsync-sync-title">
              <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">{{ syncStatusResult.loaded ? syncStatusResult.profile : 'Nothing loaded' }}</p><h3 id="sdsync-sync-title">{{ syncStatusResult.loaded ? syncStatusQueryLabel : 'Sync state' }}</h3></div><span v-if="syncStatusResult.loaded" class="sdsync-freshness">Compared by {{ syncStatusResult.compare }}</span></div>
              <p v-if="syncStatusBusy && liveProgressDetail" class="sdsync-live-progress" role="status" aria-live="polite">{{ liveProgressDetail }}</p>
              <p v-if="!syncStatusResult.loaded" class="sdsync-empty">{{ syncStatusMessage }}</p>
              <template v-else>
                <dl class="sdsync-sync-stats" aria-label="Whole-scope totals">
                  <div v-for="card in syncStatusCards" :key="card.id" :class="['sdsync-sync-stat', 'is-' + card.id]"><dt>{{ card.label }}</dt><dd>{{ card.value }}</dd></div>
                </dl>
                <p class="sdsync-field-note">{{ syncStatusTotalsNote }}</p>
                <p v-if="syncStatusMessage" class="sdsync-empty">{{ syncStatusMessage }}</p>
                <div v-else class="sdsync-table-wrap">
                  <table>
                    <thead><tr><th>Path</th><th>State</th><th>Local</th><th>On the NAS</th><th>Why</th><th><span class="sdsync-sr-only">Actions</span></th></tr></thead>
                    <tbody>
                      <tr v-for="entry in syncStatusResult.entries" :key="entry.key">
                        <td><strong class="sdsync-sync-path">{{ entry.relative }}</strong><small>{{ entry.kind }} &#183; {{ entry.remotePath }}</small></td>
                        <td><span :class="syncStateClass(entry.state)">{{ entry.label }}</span></td>
                        <td>{{ syncEntrySide(entry.localSize, entry.localEpoch) }}</td>
                        <td>{{ syncEntrySide(entry.remoteSize, entry.remoteEpoch) }}</td>
                        <td>{{ entry.detail || '&#8212;' }}</td>
                        <td><button type="button" class="sdsync-sync-row-action" :title="resyncEntryLabel(entry)" :aria-label="resyncEntryLabel(entry)" :disabled="!resyncCanPlan || entry.kind === 'directory'" @click="resyncEntry(entry)"><action-icon name="refresh" />Re-upload</button></td>
                      </tr>
                    </tbody>
                  </table>
                </div>
                <div class="sdsync-sync-pager">
                  <span class="sdsync-freshness">{{ syncStatusPageSummary }}</span>
                  <div class="sdsync-sync-pager-actions">
                    <v-button type="border" display="icon-text" tooltip="Return to the previous page" :disabled="!syncStatusHasPrevious || syncStatusBusy" @click="syncStatusPreviousPage"><template #icon><action-icon name="up" /></template>Previous page</v-button>
                    <v-button type="border" display="icon-text" tooltip="Load the next page; 200 rows at most" :disabled="!syncStatusHasNext || syncStatusBusy" @click="syncStatusNextPage"><template #icon><action-icon name="navigate" /></template>Next page</v-button>
                  </div>
                </div>
              </template>
            </article>

            <v-form v-model="resyncForm" class="sdsync-panel sdsync-resync" @submit="planResync">
              <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Force a re-upload</p><h3>Resync</h3></div><span :class="pillClass(resyncPhase === 'confirmed' ? 'running' : 'default')">{{ resyncPhase === 'confirmed' ? 'Completed' : 'Two steps' }}</span></div>
              <div class="sdsync-warning"><strong>A re-upload replaces the remote copy whatever it holds now, even an identical or newer one.</strong><span>It never deletes. The plan is shown first, and only confirming that exact plan uploads anything.</span></div>
              <div class="sdsync-filter-list" aria-label="Resync scope">
                <div class="sdsync-filter-row"><span class="sdsync-filter-label">Folder or file</span><div class="sdsync-filter-control"><v-input class="sdsync-input-control" v-model.trim="resyncForm.scope" clearable maxlength="4096" placeholder="Empty re-uploads everything in this profile" aria-label="Folder or file to re-upload" aria-describedby="sdsync-help-resync-scope" :disabled="!resyncCanPlan" /><control-help help-key="resync-scope" /></div></div>
              </div>
              <div class="sdsync-sync-actions">
                <v-button v-if="resyncPhase !== 'idle'" type="border" display="icon-text" html-type="button" tooltip="Discard this plan without uploading anything" :disabled="resyncBusy" @click="clearResyncPlan('')"><template #icon><action-icon name="close" /></template>Discard plan</v-button>
                <v-button suffix="grey" display="icon-text" html-type="submit" :tooltip="operationMutationGuidance || 'List what would be overwritten, changing nothing'" :disabled="!resyncCanPlan" :aria-busy="resyncPlanning ? 'true' : 'false'"><template #icon><action-icon :class="{ 'sdsync-is-spinning': resyncPlanning }" name="plan" /></template>Plan re-upload</v-button>
              </div>
              <p v-if="resyncMessage" :class="['sdsync-field-note', { 'is-error': resyncFailed }]">{{ resyncMessage }}</p>
              <p v-if="resyncBusy && liveProgressDetail" class="sdsync-live-progress" role="status" aria-live="polite">{{ liveProgressDetail }}</p>
              <div v-if="resyncPlan.overwrites || resyncPhase === 'confirmed'" class="sdsync-resync-plan">
                <dl class="sdsync-definition-grid">
                  <div><dt>Scope</dt><dd>{{ resyncScopeLabel }}</dd></div>
                  <div><dt>Files to overwrite</dt><dd>{{ resyncPlan.overwrites }}</dd></div>
                  <div><dt>Data to upload</dt><dd>{{ formatBytes(resyncPlan.overwriteBytes) }}</dd></div>
                  <div><dt>Ticket</dt><dd><code>{{ resyncPlan.ticket || 'Consumed' }}</code></dd></div>
                </dl>
                <p v-if="resyncPlan.staleTicket" class="sdsync-field-note is-error">Ticket {{ resyncPlan.staleTicket }} no longer matched what would be overwritten.</p>
                <ol class="sdsync-resync-paths">
                  <li v-for="path in resyncPlan.paths" :key="path.key"><code>{{ path.relative }}</code><small>{{ formatBytes(path.bytes) }}</small></li>
                </ol>
                <p v-if="resyncPlan.truncatedPaths" class="sdsync-field-note">First {{ resyncPlan.paths.length }} paths only; the totals above cover the whole plan.</p>
                <div class="sdsync-sync-actions">
                  <v-button suffix="red" display="icon-text" html-type="button" :tooltip="resyncPlanReady ? 'Upload exactly the files listed above' : 'Plan a re-upload before confirming'" :disabled="!resyncPlanReady" :aria-busy="resyncConfirming ? 'true' : 'false'" @click="confirmResync"><template #icon><action-icon :class="{ 'sdsync-is-spinning': resyncConfirming }" name="confirm" /></template>Confirm this exact plan</v-button>
                </div>
              </div>
            </v-form>
          </section>

          <section v-else-if="route === 'health'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <div class="sdsync-doctor-layout">
              <v-form v-model="doctorForm" class="sdsync-panel sdsync-horizontal-form sdsync-doctor-form" direction="horizontal" @submit="runDoctor">
                <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Target doctor</p><h3>Choose diagnostic depth</h3></div><span class="sdsync-pill neutral">Manual</span></div>
                <v-form-item class="sdsync-form-item sdsync-inline-form-item" label="Scope" label-flex="0 0 150px" control-flex="1 1 auto"><template #label-after><control-help class="sdsync-form-label-help" help-key="doctor-scope" /></template><v-single-select class="sdsync-select-control" v-model="doctorForm.scope" :options="scopeOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-doctor-scope" :disabled="!canRunOperations"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                <v-form-item class="sdsync-form-item sdsync-inline-form-item" label="Test level" label-flex="0 0 150px" control-flex="1 1 auto"><template #label-after><control-help class="sdsync-form-label-help" help-key="doctor-level" /></template><v-single-select class="sdsync-select-control" v-model="doctorForm.level" :options="doctorLevelOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-doctor-level" :disabled="!canRunOperations || doctorForm.write_test"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item>
                <aside :class="['sdsync-doctor-level-note', 'is-' + doctorForm.level]" role="note"><strong>{{ doctorLevelTitle }}</strong><span>{{ doctorLevelGuidance }}</span><small v-if="doctorForm.level === 'extensive'">Extensive remains read-only unless the separate disposable write probe is enabled and confirmed.</small></aside>
                <div class="sdsync-toggle-row"><span class="sdsync-toggle-label">Disposable write test <control-help help-key="doctor-write" /></span><v-checkbox class="sdsync-checkbox-control" v-model="doctorForm.write_test" aria-label="Disposable write test" aria-describedby="sdsync-help-doctor-write" :disabled="!canRunOperations || !canRunDoctorWrite || !hasCapability('write_test')" @input="onDoctorWriteTestChanged" /></div>
                <div v-if="doctorForm.write_test" class="sdsync-warning"><strong>Write probe enabled; test level is locked to Extensive.</strong><span>A unique probe is created, verified, and removed. All other Extensive checks remain non-mutating.</span><div class="sdsync-toggle-row"><span class="sdsync-toggle-label">I prepared a non-critical destination and approve probe cleanup <control-help help-key="doctor-write-confirm" /></span><v-checkbox class="sdsync-checkbox-control" v-model="doctorForm.write_confirm" aria-label="Approve disposable probe cleanup" aria-describedby="sdsync-help-doctor-write-confirm" :disabled="!canRunOperations || !canRunDoctorWrite" /></div></div>
                <div v-if="doctorProgress.active" class="sdsync-doctor-progress" role="status" aria-live="polite" aria-label="Target Doctor progress">
                  <div class="sdsync-doctor-progress-heading"><action-icon class="sdsync-is-spinning" name="refresh" :size="20" /><span><strong>Doctor is running</strong><small>Keep this AppWindow open while terminal evidence is collected.</small></span></div>
                  <p v-if="liveProgressDetail" class="sdsync-live-progress">{{ liveProgressDetail }}</p>
                  <div class="sdsync-doctor-progress-track" aria-hidden="true"><span /></div>
                  <ol><li v-for="stage in doctorProgressStages" :key="stage.id" :class="'is-' + stage.state"><span class="sdsync-doctor-state-dot" />{{ stage.label }}<small>{{ doctorStatusLabel(stage.state) }}</small></li></ol>
                </div>
                <div class="sdsync-submit-row"><v-button suffix="main" display="icon-text" html-type="submit" :tooltip="operationMutationGuidance || 'Run bounded target checks and wait for terminal evidence'" :disabled="!canRunOperations || operationBusy" :aria-busy="doctorProgress.active ? 'true' : 'false'"><template #icon><action-icon :class="{ 'sdsync-is-spinning': doctorProgress.active }" name="doctor" /></template>Run doctor</v-button></div>
              </v-form>
              <article class="sdsync-panel sdsync-doctor-session" aria-live="polite">
                <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Latest diagnostic</p><h3>{{ diagnostic.title }}</h3></div><v-button type="border" display="icon-text" aria-label="Copy Target Doctor diagnostics" tooltip="Copy this bounded, credential-redacted diagnostic breakdown" :disabled="!doctorCopyAvailable" @click="copyDoctorDiagnostics"><template #icon><action-icon name="copy" /></template>Copy diagnostics</v-button></div>
                <div :class="['sdsync-doctor-overall', doctorStatusClass(doctorReport.state)]"><span class="sdsync-doctor-state-dot" /><div><strong>{{ doctorStatusLabel(doctorReport.state) }}</strong><small>{{ doctorReport.structured ? 'Structured section evidence' : 'Legacy terminal output fallback' }} · {{ doctorReport.level }} level<span v-if="doctorReport.duration_ms !== null"> · {{ formatDuration(doctorReport.duration_ms) }}</span></small></div></div>
                <dl class="sdsync-doctor-summary" aria-label="Diagnostic result counts"><div v-for="item in doctorSummaryCards" :key="item.state" :class="doctorStatusClass(item.state)"><dt>{{ item.label }}</dt><dd>{{ item.count }}</dd></div></dl>
                <p class="sdsync-doctor-session-note">{{ doctorReportNote }}</p>
              </article>
            </div>
            <article class="sdsync-panel sdsync-doctor-breakdown" aria-labelledby="sdsync-doctor-breakdown-title">
              <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Section evidence</p><h3 id="sdsync-doctor-breakdown-title">Negotiation-to-target breakdown</h3></div><span class="sdsync-freshness">{{ doctorReport.sections.length }} section{{ doctorReport.sections.length === 1 ? '' : 's' }}</span></div>
              <div v-if="doctorCleanupWarning" class="sdsync-warning sdsync-doctor-cleanup-warning" role="alert"><strong>Disposable probe cleanup needs attention.</strong><span>{{ doctorCleanupWarning }}</span></div>
              <p v-if="!doctorReport.sections.length" class="sdsync-empty">Run Target Doctor to see OK, warning, not OK, and skipped evidence for each diagnostic area.</p>
              <ol v-else class="sdsync-doctor-sections">
                <li v-for="(section, sectionIndex) in doctorReport.sections" :key="[section.profile, section.id, sectionIndex].join(':')" :class="doctorStatusClass(section.state)">
                  <div class="sdsync-doctor-section-heading"><span class="sdsync-doctor-state-dot" /><div><strong>{{ section.label }}</strong><small v-if="section.step">Step {{ section.step }} of {{ doctorStepTotal }}</small><small v-if="section.profile">Profile: {{ section.profile }}</small><small v-if="section.timing_scope">Timing scope: {{ section.timing_scope }}</small></div><span class="sdsync-doctor-state-label">{{ doctorStatusLabel(section.state) }}</span><time v-if="section.duration_ms !== null">{{ formatDuration(section.duration_ms) }}</time></div>
                  <p>{{ section.detail }}</p>
                  <ul v-if="section.checks.length" class="sdsync-doctor-checks"><li v-for="check in section.checks" :key="check.id" :class="doctorStatusClass(check.state)"><span class="sdsync-doctor-state-dot" /><strong>{{ check.label }}</strong><span>{{ check.detail }}</span><time v-if="check.duration_ms !== null">{{ formatDuration(check.duration_ms) }}</time><small>{{ doctorStatusLabel(check.state) }}</small></li></ul>
                  <div v-if="section.inventory" class="sdsync-doctor-inventory"><div class="sdsync-doctor-inventory-summary"><strong>{{ section.inventory.total }} remote entr{{ section.inventory.total === 1 ? 'y' : 'ies' }} reported</strong><span>{{ doctorInventoryScopeLabel(section.inventory.scope) }} · {{ section.inventory.entries.length }} displayed<span v-if="section.inventory.truncated"> · bounded sample truncated</span></span></div><p v-if="!section.inventory.entries.length" class="sdsync-doctor-inventory-empty">No logical entries were visible in this scope.</p><ul v-else><li v-for="(entry, entryIndex) in section.inventory.entries" :key="entry.path + ':' + entryIndex"><span class="sdsync-doctor-entry-kind">{{ entry.kind }}</span><div><strong>{{ entry.path }}</strong><small>{{ doctorInventoryMetadata(entry) }}</small></div></li></ul></div>
                </li>
              </ol>
              <details v-if="diagnostic.output" class="sdsync-doctor-raw"><summary>{{ doctorReport.structured ? 'Structured terminal summary' : 'Raw terminal evidence' }}</summary><pre>{{ diagnostic.output }}</pre></details>
            </article>
            <article class="sdsync-panel"><div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Cached per-profile evidence</p><h3>Target health</h3></div><span class="sdsync-freshness">{{ healthFreshness }}</span></div><div class="sdsync-table-wrap"><table><thead><tr><th>Profile</th><th>Last check</th><th>Reachable</th><th>Auth</th><th>Writable</th><th>Latency</th><th>Last success</th><th>Doctor</th><th>Free space</th></tr></thead><tbody><tr v-if="!healthRows.length"><td colspan="9">No cached target-health evidence.</td></tr><tr v-for="health in healthRows" :key="health.profile"><td>{{ health.profile || 'Unknown' }}</td><td>{{ formatDate(health.last_check_epoch || health.checked_at_epoch || health.checked_epoch) }}</td><td :class="healthClass(health.reachable)">{{ booleanEvidence(health.reachable) }}</td><td :class="healthClass(health.authenticated !== undefined ? health.authenticated : health.auth)">{{ booleanEvidence(health.authenticated !== undefined ? health.authenticated : health.auth) }}</td><td :class="healthClass(health.writable)">{{ booleanEvidence(health.writable) }}</td><td>{{ formatDuration(health.latency_ms) }}</td><td>{{ formatDate(health.last_success_epoch || health.last_successful_sync_epoch) }}</td><td>{{ health.doctor_status || health.last_doctor_status || health.state || 'Unavailable' }}</td><td>{{ health.free_space_proven === true ? formatBytes(health.free_space_bytes) : 'Unavailable' }}</td></tr></tbody></table></div></article>
          </section>

          <section v-else-if="route === 'activity'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <div class="sdsync-page-actions"><v-button suffix="grey" display="icon-text" tooltip="Pause or resume browser-side log refreshes" @click="toggleLogs"><template #icon><action-icon :name="logsPaused ? 'run' : 'pause'" /></template>{{ logsPaused ? 'Resume live updates' : 'Pause live updates' }}</v-button><v-button suffix="grey" display="icon-text" tooltip="Clear only this rendered view; package logs remain intact" @click="clearLogView"><template #icon><action-icon name="clear" /></template>Clear view</v-button></div>
            <article class="sdsync-panel">
              <div class="sdsync-panel-heading">
                <div><p class="sdsync-eyebrow">Structured activity</p><h3>Recent package events</h3></div>
                <div class="sdsync-evidence-heading-actions"><span class="sdsync-freshness">{{ reversedActivity.length }} of {{ activityEvents.length }} event{{ activityEvents.length === 1 ? '' : 's' }}</span><v-button type="border" display="icon-text" aria-label="Copy all visible activity events" tooltip="Copy the filtered activity events as bounded, sanitized troubleshooting text" :disabled="!reversedActivity.length" @click="copyVisibleActivity"><template #icon><action-icon name="copy" /></template>Copy visible</v-button></div>
              </div>
              <div class="sdsync-filter-list" aria-label="Activity filters">
                <div class="sdsync-filter-row"><span class="sdsync-filter-label">Search</span><div class="sdsync-filter-control"><v-input v-model.trim="activitySearch" class="sdsync-input-control sdsync-activity-search" maxlength="128" placeholder="Search event text or request ID" aria-label="Search activity text or client request ID" aria-describedby="sdsync-help-activity-search" /><control-help help-key="activity-search" /></div></div>
                <div class="sdsync-filter-row"><span class="sdsync-filter-label">Category</span><div class="sdsync-filter-control"><v-single-select class="sdsync-select-control" v-model="activityCategory" :options="activityCategoryOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-label="Activity category" aria-describedby="sdsync-help-activity-category"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select><control-help help-key="activity-category" /></div></div>
                <div class="sdsync-filter-row"><span class="sdsync-filter-label">Level</span><div class="sdsync-filter-control"><v-single-select class="sdsync-select-control" v-model="activityLevel" :options="activityLevelOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-label="Activity level" aria-describedby="sdsync-help-activity-level"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select><control-help help-key="activity-level" /></div></div>
              </div>
              <ol class="sdsync-activity-feed"><li v-if="!reversedActivity.length" class="sdsync-empty">No package events match these filters.</li><li v-for="event in reversedActivity" :key="[event.epoch, event.code, event.profile, event.category, event.level, event.client_request_id].join(':')"><time>{{ formatDate(event.epoch) }}</time><div class="sdsync-activity-detail"><strong>{{ event.code }}</strong><p v-if="event.message && !event.doctor_inventory">{{ event.message }}</p><div v-if="event.doctor_inventory" class="sdsync-inventory-evidence"><div class="sdsync-inventory-evidence-summary"><strong>{{ doctorInventoryScopeLabel(event.doctor_inventory.inventory.scope) }}</strong><span>{{ event.doctor_inventory.inventory.total }} total · {{ event.doctor_inventory.inventory.entries.length }} shown<span v-if="event.doctor_inventory.inventory.truncated"> · truncated</span></span></div><p v-if="!event.doctor_inventory.inventory.entries.length">No logical entries were visible.</p><div v-for="(entry, index) in event.doctor_inventory.inventory.entries" :key="entry.path + ':' + index" class="sdsync-inventory-evidence-entry"><span>{{ entry.kind }}</span><code>{{ entry.path }}</code><small>{{ entry.name }}</small></div></div><code v-if="event.client_request_id">Client request ID: {{ event.client_request_id }}</code></div><small>{{ event.profile }} · {{ event.state }} · {{ event.category }} / {{ event.level }}</small><v-button class="sdsync-evidence-copy" type="border" display="icon-text" :aria-label="'Copy activity event ' + event.code" tooltip="Copy this event as bounded, sanitized troubleshooting text" @click="copyActivityEvent(event)"><template #icon><action-icon name="copy" /></template>Copy</v-button></li></ol>
            </article>
            <article class="sdsync-panel sdsync-log-panel">
              <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Bounded package logs</p><h3>Troubleshooting evidence</h3></div><div class="sdsync-evidence-heading-actions"><span class="sdsync-log-state">{{ logState }}</span><v-button type="border" display="icon-text" aria-label="Copy all visible package logs" tooltip="Copy the selected log sources as bounded, sanitized troubleshooting text" :disabled="!logRecords.length" @click="copyVisibleLogs"><template #icon><action-icon name="copy" /></template>Copy visible</v-button></div></div>
              <div class="sdsync-filter-list sdsync-log-filters" aria-label="Log filters"><div class="sdsync-filter-row"><span class="sdsync-filter-label">Source</span><div class="sdsync-filter-control"><v-single-select class="sdsync-select-control" v-model="logSource" :options="logSourceOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-label="Log source" aria-describedby="sdsync-help-log-source" @input="refreshLogs"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select><control-help help-key="log-source" /></div></div><div class="sdsync-filter-row"><span class="sdsync-filter-label">Lines</span><div class="sdsync-filter-control"><v-single-select class="sdsync-select-control" v-model="logLines" :options="logLineOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-label="Log line count" aria-describedby="sdsync-help-log-lines" @input="refreshLogs"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select><control-help help-key="log-lines" /></div></div></div>
              <p v-if="!logRecords.length" class="sdsync-empty">{{ logOutput }}</p>
              <div v-else class="sdsync-log-records"><section v-for="record in logRecords" :key="record.id" class="sdsync-log-record"><header><span><strong>{{ record.source }}</strong><small>{{ record.lineCount }} line{{ record.lineCount === 1 ? '' : 's' }}</small></span><span class="sdsync-log-record-actions"><v-button class="sdsync-evidence-copy" type="border" display="icon-text" :aria-label="'Copy ' + record.source + ' log evidence'" tooltip="Copy this log record as bounded, sanitized troubleshooting text" @click="copyLogRecord(record)"><template #icon><action-icon name="copy" /></template>Copy</v-button><v-button class="sdsync-evidence-copy sdsync-log-clear" type="border" display="icon-text" :aria-label="'Clear the ' + record.source + ' package log'" :tooltip="logClearTooltip(record.source)" :disabled="!logSourceClearable(record.source) || !canRunOperations" @click="clearLogSource(record.source)"><template #icon><action-icon name="delete" /></template>Clear</v-button></span></header><div v-if="record.doctorInventories.length" class="sdsync-log-inventory-evidence"><div v-for="(inventoryRecord, recordIndex) in record.doctorInventories" :key="inventoryRecord.epoch + ':' + inventoryRecord.profile + ':' + recordIndex" class="sdsync-inventory-evidence"><div class="sdsync-inventory-evidence-summary"><strong>{{ inventoryRecord.profile }} · {{ doctorInventoryScopeLabel(inventoryRecord.inventory.scope) }}</strong><span>{{ inventoryRecord.inventory.total }} total · {{ inventoryRecord.inventory.entries.length }} shown<span v-if="inventoryRecord.inventory.truncated"> · truncated</span></span></div><p v-if="!inventoryRecord.inventory.entries.length">No logical entries were visible.</p><div v-for="(entry, index) in inventoryRecord.inventory.entries" :key="entry.path + ':' + index" class="sdsync-inventory-evidence-entry"><span>{{ entry.kind }}</span><code>{{ entry.path }}</code><small>{{ entry.name }}</small></div></div></div><p v-if="!record.lines.length" class="sdsync-empty sdsync-log-empty">No lines to show. This log can still hold records that the current log level for its category keeps out of this view.</p><ol v-else class="sdsync-log-lines" tabindex="0"><li v-for="line in record.lines" :key="line.id"><time v-if="line.epoch">{{ formatDate(line.epoch) }}</time><time v-else class="is-unrecorded" title="This line was written without a recorded time; the package now stamps every record it writes.">Time not recorded</time><span>{{ line.text }}</span></li></ol></section></div>
            </article>
          </section>

          <section v-else-if="route === 'notifications'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <div class="sdsync-subtabs" data-subtabs="notifications" role="tablist" aria-label="Notification settings" @keydown="moveSubtab('notificationTab', notificationTabs, $event)">
              <button v-for="tab in notificationTabs" :id="'sdsync-notifications-tab-' + tab.id" :key="tab.id" type="button" :class="['sdsync-subtab', { 'is-active': notificationTab === tab.id }]" :data-subtab="tab.id" role="tab" :aria-selected="notificationTab === tab.id" :aria-controls="'sdsync-notifications-panel-' + tab.id" :tabindex="notificationTab === tab.id ? 0 : -1" @click="notificationTab = tab.id">{{ tab.label }}</button>
            </div>
            <div class="sdsync-subtab-stage">
              <transition name="sdsync-subtab-swap" mode="out-in">
                <div v-if="notificationTab === 'package-alerts'" id="sdsync-notifications-panel-package-alerts" key="package-alerts" class="sdsync-subtab-panel" data-subtab-panel="package-alerts" role="tabpanel" aria-labelledby="sdsync-notifications-tab-package-alerts" tabindex="0">
                  <v-form v-model="alertForm" class="sdsync-panel sdsync-horizontal-form sdsync-alert-form" direction="horizontal" @submit="saveAlerts">
                    <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">DSM desktop alerts</p><h3>Package alert policy</h3></div><span :class="pillClass(alertForm.enabled ? 'running' : 'disabled')">{{ alertForm.enabled ? 'Enabled' : 'Disabled' }}</span></div>
                    <div class="sdsync-toggle-row"><span class="sdsync-toggle-label">Enable DSM desktop alerts <control-help help-key="alerts-enabled" /></span><v-checkbox class="sdsync-checkbox-control" v-model="alertForm.enabled" aria-label="Enable DSM desktop alerts" aria-describedby="sdsync-help-alerts-enabled" :disabled="!canChangeNotifications" /></div>
                    <div class="sdsync-toggle-row"><span class="sdsync-toggle-label">Notify on success <control-help help-key="alerts-success" /></span><v-checkbox class="sdsync-checkbox-control" v-model="alertForm.on_success" aria-label="Notify on success" aria-describedby="sdsync-help-alerts-success" :disabled="!canChangeNotifications" /></div>
                    <div class="sdsync-toggle-row"><span class="sdsync-toggle-label">Notify on failure <control-help help-key="alerts-failure" /></span><v-checkbox class="sdsync-checkbox-control" v-model="alertForm.on_failure" aria-label="Notify on failure" aria-describedby="sdsync-help-alerts-failure" :disabled="!canChangeNotifications" /></div>
                    <v-form-item class="sdsync-form-item sdsync-inline-form-item" label="Failures before alert" label-flex="0 0 150px" control-flex="1 1 auto"><template #label-after><control-help class="sdsync-form-label-help" help-key="alerts-threshold" /></template><v-input class="sdsync-input-control" v-model="alertForm.failure_threshold" number-only :disabled="!canChangeNotifications" aria-describedby="sdsync-help-alerts-threshold" /></v-form-item>
                    <v-form-item class="sdsync-form-item sdsync-inline-form-item" label="Cooldown (seconds)" label-flex="0 0 150px" control-flex="1 1 auto"><template #label-after><control-help class="sdsync-form-label-help" help-key="alerts-cooldown" /></template><v-input class="sdsync-input-control" v-model="alertForm.cooldown_seconds" number-only :disabled="!canChangeNotifications" aria-describedby="sdsync-help-alerts-cooldown" /></v-form-item>
                    <div class="sdsync-submit-row"><span class="sdsync-field-note">Valid changes autosave after 1.3 seconds.</span><v-button suffix="main" display="icon-text" html-type="submit" :tooltip="alertsOutcomeUnresolved ? alertsOutcomeGuidance : 'Validate and persist the package-level DSM alert policy immediately'" :disabled="!canSubmitAlerts"><template #icon><action-icon name="save" /></template>{{ alertsOutcomeUnresolved ? 'Save locked' : 'Save now' }}</v-button></div>
                  </v-form>
                </div>
                <div v-else id="sdsync-notifications-panel-session-preferences" key="session-preferences" class="sdsync-subtab-panel" data-subtab-panel="session-preferences" role="tabpanel" aria-labelledby="sdsync-notifications-tab-session-preferences" tabindex="0">
                  <v-form v-model="notificationForm" class="sdsync-panel" direction="vertical" @submit="saveNotificationPreferences">
                    <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Open-session signal</p><h3>Browser fallback</h3></div><span :class="pillClass(notificationPermission)">{{ notificationPermission }}</span></div>
                    <div class="sdsync-toggle-row"><span class="sdsync-toggle-label">Notify while this app is open <control-help help-key="session-notify" /></span><v-checkbox class="sdsync-checkbox-control" v-model="notificationForm.desktop_notifications" aria-label="Notify while this app is open" aria-describedby="sdsync-help-session-notify" :disabled="!canChangeNotifications" /></div>
                    <div class="sdsync-toggle-row"><span class="sdsync-toggle-label">Audible cue <control-help help-key="session-audible" /></span><v-checkbox class="sdsync-checkbox-control" v-model="notificationForm.audible" aria-label="Use an audible cue" aria-describedby="sdsync-help-session-audible" :disabled="!canChangeNotifications" /></div>
                    <div class="sdsync-submit-row"><v-button suffix="grey" display="icon-text" html-type="submit" :tooltip="interfaceOutcomeUnresolved ? interfaceOutcomeGuidance : 'Save and audit non-secret notification preferences in this browser'" :disabled="!canSubmitNotificationPreferences"><template #icon><action-icon name="save" /></template>{{ interfaceOutcomeUnresolved ? 'Save locked' : 'Save session preferences' }}</v-button></div>
                  </v-form>
                </div>
              </transition>
            </div>
          </section>

          <section v-else-if="route === 'security'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <security-panel :value="securityForm" :disabled="!canMutate" :busy="operationBusy" :dirty="securityDirty" :save-blocked="securityOutcomeUnresolved" :save-blocked-message="securityOutcomeGuidance" :theme-class="themeClass" :log-level-options="logLevelOptions" @input="updateSecurityForm" @save="saveSecurityPolicy" />
          </section>

          <section v-else-if="route === 'settings'" class="sdsync-page" aria-labelledby="sdsync-page-title">
            <v-form v-model="settings" class="sdsync-panel sdsync-settings-panel" direction="horizontal" @submit="saveInterfaceSettings"><div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Display and refresh</p><h3>Interface</h3></div></div><v-form-item class="sdsync-form-item" label="Theme"><template #label-after><control-help class="sdsync-form-label-help" help-key="settings-theme" /></template><v-single-select class="sdsync-select-control" v-model="settings.theme" :options="themeOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-settings-theme" :disabled="!canChangeInterface"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item><v-form-item class="sdsync-form-item" label="Status refresh"><template #label-after><control-help class="sdsync-form-label-help" help-key="settings-status-refresh" /></template><v-single-select class="sdsync-select-control" v-model="settings.status_refresh" :options="statusRefreshOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-settings-status-refresh" :disabled="!canChangeInterface"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item><v-form-item class="sdsync-form-item" label="Log refresh"><template #label-after><control-help class="sdsync-form-label-help" help-key="settings-log-refresh" /></template><v-single-select class="sdsync-select-control" v-model="settings.log_refresh" :options="logRefreshOptions" width="100%" :custom-dropdown-cls="'sdsync-select-dropdown ' + themeClass" aria-describedby="sdsync-help-settings-log-refresh" :disabled="!canChangeInterface"><template #dropdown-icon><action-icon name="chevron-down" /></template></v-single-select></v-form-item><div class="sdsync-form-actions sdsync-settings-actions"><span /><span class="sdsync-field-note">Valid changes autosave after 1.3 seconds.</span><v-button suffix="main" display="icon-text" html-type="submit" :tooltip="interfaceOutcomeUnresolved ? interfaceOutcomeGuidance : 'Apply, persist, and audit this browser AppWindow preferences immediately'" :disabled="!canSubmitInterface"><template #icon><action-icon name="save" /></template>{{ interfaceOutcomeUnresolved ? 'Save locked' : 'Save now' }}</v-button></div></v-form>
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
        <div v-if="pathBrowser.visible" class="sdsync-modal-backdrop sdsync-path-browser-backdrop" role="presentation" @click.self="closePathBrowser">
          <div ref="pathBrowserDialog" class="sdsync-modal sdsync-path-browser" role="dialog" aria-modal="true" aria-labelledby="sdsync-path-browser-title" aria-describedby="sdsync-path-browser-description" tabindex="-1">
            <header class="sdsync-path-browser-header">
              <div class="sdsync-panel-heading"><div><p class="sdsync-eyebrow">Folder explorer</p><h2 id="sdsync-path-browser-title">{{ pathBrowser.kind === 'local' ? 'Choose a local NAS source' : 'Choose a File Station target' }}</h2></div><v-button ref="pathBrowserClose" type="border" display="icon-text" tooltip="Close the folder explorer and keep manual path entry" @click="closePathBrowser"><template #icon><action-icon name="close" /></template>Close</v-button></div>
            </header>
            <p id="sdsync-path-browser-description" class="sdsync-path-browser-intro">{{ pathBrowser.kind === 'local' ? 'Only canonical internal, USB, and SATA DSM volume folders readable and traversable by the package identity are shown.' : 'Only folders returned to the successfully authenticated DSM account are shown.' }}</p>
            <div class="sdsync-path-browser-toolbar">
              <v-button type="border" display="icon-text" tooltip="Go up to the parent folder" aria-label="Go up to the parent folder" :disabled="pathBrowser.loading || !pathBrowser.parent" @click="browsePath(pathBrowser.parent)"><template #icon><action-icon name="up" /></template>Up one level</v-button>
              <nav class="sdsync-path-browser-breadcrumbs" aria-label="Current folder">
                <ol>
                  <li v-for="crumb in pathBrowserBreadcrumbs(pathBrowser.current)" :key="crumb.path"><button type="button" class="sdsync-path-browser-crumb" :aria-label="'Open ' + crumb.label" :aria-current="crumb.current ? 'location' : null" :disabled="pathBrowser.loading || crumb.current" @click="browsePath(crumb.path)">{{ crumb.label }}</button><span v-if="!crumb.current" class="sdsync-path-browser-separator" aria-hidden="true">/</span></li>
                </ol>
              </nav>
            </div>
            <section class="sdsync-path-browser-main" aria-label="Folder contents">
              <div class="sdsync-path-browser-current" aria-live="polite"><action-icon name="folder" size="20" /><span class="sdsync-path-browser-current-copy"><span class="sdsync-path-browser-current-label">Current folder</span><code>{{ pathBrowser.current }}</code></span></div>
              <div class="sdsync-path-browser-columns" aria-hidden="true"><span>Name</span><span>Choose</span></div>
              <div class="sdsync-path-browser-list" :aria-busy="pathBrowser.loading ? 'true' : 'false'">
                <div v-if="pathBrowser.loading" class="sdsync-path-browser-state" role="status" aria-live="polite"><action-icon class="sdsync-is-spinning" name="refresh" size="22" /><strong>Opening folder</strong><span>Reading the folders visible at <code>{{ pathBrowser.current }}</code>…</span></div>
                <div v-else-if="pathBrowser.error && !pathBrowser.directories.length" class="sdsync-path-browser-state sdsync-path-browser-error" role="alert"><action-icon name="about" size="22" /><strong>Folder listing unavailable</strong><span>{{ pathBrowser.error }}</span><v-button type="border" display="icon-text" tooltip="Retry this exact folder listing" :disabled="pathBrowser.kind === 'remote' && profileConnectionBlocked" @click="browsePath(pathBrowser.current)"><template #icon><action-icon name="refresh" /></template>Retry current folder</v-button></div>
                <div v-else class="sdsync-path-browser-entries" role="list">
                  <div v-if="pathBrowser.error" class="sdsync-path-browser-evidence" role="listitem"><span role="alert">{{ pathBrowser.error }}</span></div>
                  <div v-for="directory in pathBrowser.directories" :key="directory.path" class="sdsync-path-browser-row" role="listitem"><button type="button" class="sdsync-path-browser-open" :aria-label="'Open folder ' + directory.name" @click="browsePath(directory.path)"><span class="sdsync-path-browser-folder-icon"><action-icon name="folder" size="18" /></span><span class="sdsync-path-browser-folder-copy"><strong class="sdsync-path-browser-folder-name">{{ directory.name }}</strong><code class="sdsync-path-browser-folder-path">{{ directory.path }}</code></span><action-icon name="navigate" /></button><v-button type="border" display="icon-text" :aria-label="'Select folder ' + directory.name" tooltip="Select this folder without opening it" :disabled="pathBrowser.loading" @click.stop="selectPath(directory.path)"><template #icon><action-icon name="confirm" /></template>Select</v-button></div>
                  <div v-if="!pathBrowser.directories.length" class="sdsync-path-browser-state" role="listitem"><action-icon name="folder" size="22" /><strong>No child folders visible</strong><span>{{ pathBrowser.kind === 'local' ? 'Grant the package identity access in DSM or choose the current folder.' : 'Choose the current folder or navigate up to another location.' }}</span></div>
                </div>
              </div>
            </section>
            <footer class="sdsync-path-browser-footer">
              <span class="sdsync-path-browser-summary"><span v-if="pathBrowser.loading">Reading folder…</span><span v-else-if="pathBrowser.error && !pathBrowser.directories.length">Listing unavailable</span><span v-else>{{ pathBrowser.directories.length }} child folder{{ pathBrowser.directories.length === 1 ? '' : 's' }} visible<span v-if="pathBrowser.truncated"> · bounded result truncated</span></span><code>{{ pathBrowser.current }}</code></span>
              <span class="sdsync-path-browser-footer-actions"><v-button suffix="cancel" display="icon-text" tooltip="Close without changing the profile path" @click="closePathBrowser"><template #icon><action-icon name="close" /></template>Cancel</v-button><v-button suffix="main" display="icon-text" tooltip="Use the current folder in this profile" :disabled="pathBrowser.loading || pathBrowser.current === '/'" @click="selectPath(pathBrowser.current)"><template #icon><action-icon name="confirm" /></template>Select this folder</v-button></span>
            </footer>
          </div>
        </div>
      </div>
    </v-app-window>
  </v-app-instance>
</template>

<script>
import { ActionIcon } from "./ActionIcon";
import { createAutosaveCoordinator } from "./autosave";
import { installControlLayout } from "./controlLayout";
import {
  ACTIONS,
  AUTOSAVE_API_LIMITS,
  MAX_RESPONSE_BYTES,
  PROGRESS_UNAVAILABLE,
  QueuedOutcomeUnknownError,
  SNAPSHOT_SCHEMA,
  SYNC_STATUS_MAX_LIMIT,
  apiGet,
  apiPost,
  trustedRequestProgress,
  probeRequestOutcome,
  purgeReconciliationAuth,
  reconcileMutationRequest,
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
// Which bounded package logs the dashboard may empty. The audit log is absent
// on purpose and the bridge refuses it independently: it is the record of who
// cleared what, so offering a button that erases the evidence of its own use
// would make the audit trail unable to answer the one question it exists for.
const CLEARABLE_LOG_SOURCES = Object.freeze(["api", "controller", "doctor", "scheduler", "sync"]);
const AUTOSAVE_SCOPES = Object.freeze(["profile", "routine", "alerts", "security", "interface"]);
const INCIDENT_SCOPE_LABELS = Object.freeze({
  profile: "Profile configuration and secrets",
  routine: "Routines",
  alerts: "Package alerts",
  security: "Security policy",
  interface: "Interface and session preferences",
  connection: "Authentication testing and File Station browsing",
  operations: "Run and Doctor operations"
});
const PROFILE_SECRET_KINDS = Object.freeze(["password", "totp", "remote-log-token"]);
const PROFILE_CREATION_WINDOW_WARNING = "Keep this AppWindow open; do not navigate away until profile creation finishes.";
const PROFILE_CONNECTION_HEALTHY_TIMING = "On a healthy path, allow up to 15 seconds once dispatched; queued-result polling can continue shortly after. Controller or service failures may settle differently.";
const PROFILE_CONNECTION_API_LIMITS = Object.freeze({
  csrfReissueTimeoutMs: 10000,
  postRequestTimeoutMs: 45000,
  postResponseTimeoutMs: 10000,
  readTimeoutMs: 30000,
  resultRequestTimeoutMs: 15000,
  resultObservationTimeoutMs: 120000,
  requestReconciliationTimeoutMs: 45000,
  requestReconciliationPollIntervalMs: 1000
});
const DOCTOR_LEVELS = Object.freeze(["quick", "standard", "extensive"]);
const DOCTOR_OUTPUT_LIMIT_BYTES = 1024 * 1024;
// Mirror of DOCTOR_SECTION_SPECS in src/lib.rs, in the same order. The two are
// asserted to carry the same ids by test_synology_ui.py, because nothing else
// relates them and a stale list here silently under-reports a diagnostic run.
//
// Order is the CLI's, which is thematic rather than execution order — File
// Station capabilities are settled from the discovery response long before
// authentication, yet belong beside the other File Station checks. The `step`
// each section carries is what conveys execution order, and it is rendered.
//
// Every `minimum` is "quick" on purpose: the CLI builds all sixteen sections up
// front as skipped and emits every id at every level, so there is no per-section
// minimum on the Rust side to mirror. Do not invent one here.
const DOCTOR_SECTION_CATALOG = Object.freeze([
  Object.freeze({ id: "network_reachability", label: "Network reachability and connect timing", minimum: "quick", detail: "Measure TCP reachability and connect timing before any protocol negotiation." }),
  Object.freeze({ id: "routing_tls", label: "Routing and TLS negotiation", minimum: "quick", detail: "Resolve the endpoint and negotiate the configured HTTPS transport." }),
  Object.freeze({ id: "dsm_api_discovery", label: "DSM API discovery", minimum: "quick", detail: "Negotiate a compatible DSM and File Station API surface." }),
  Object.freeze({ id: "capability_enumeration", label: "DSM capability enumeration", minimum: "quick", detail: "Enumerate the API surface the target advertises." }),
  Object.freeze({ id: "intermediary_transport", label: "Intermediaries and reverse proxies", minimum: "quick", detail: "Summarise proxy and intermediary behaviour observed across the whole run." }),
  Object.freeze({ id: "dsm_session_auth", label: "DSM session authentication", minimum: "standard", detail: "Authenticate a temporary target session without exposing credentials." }),
  Object.freeze({ id: "session_channel_ablation", label: "DSM session channel ablation", minimum: "standard", detail: "Establish which session channel the target actually honours." }),
  Object.freeze({ id: "session_concurrency", label: "Concurrent session fan-out", minimum: "standard", detail: "Check whether concurrent authenticated requests keep their session." }),
  Object.freeze({ id: "session_cookie_ledger", label: "Session cookie permanence", minimum: "standard", detail: "Summarise how the target's session cookies behaved across the whole run." }),
  Object.freeze({ id: "file_station_capabilities", label: "File Station capabilities", minimum: "standard", detail: "Check the target operations required by this profile." }),
  Object.freeze({ id: "capability_diagnosis", label: "File Station capability diagnosis", minimum: "standard", detail: "Explain any capability the target declined to offer." }),
  Object.freeze({ id: "destination_path_resolution", label: "Destination path resolution", minimum: "standard", detail: "Resolve the configured destination to an exact shared folder and path." }),
  Object.freeze({ id: "destination_permissions", label: "Destination permissions", minimum: "standard", detail: "With a configured destination, verify child-create/write permission at the exact path or its nearest existing ancestor; otherwise skip this section." }),
  Object.freeze({ id: "destination_inventory", label: "Destination inventory", minimum: "standard", detail: "With a configured destination, inspect a bounded direct-child sample; otherwise sample visible shared-folder roots without selecting or traversing a share." }),
  Object.freeze({ id: "disposable_write_verify_cleanup", label: "Disposable write, verify, and cleanup", minimum: "write", detail: "Create, verify, and remove one explicitly approved probe." }),
  Object.freeze({ id: "session_logout", label: "DSM session logout", minimum: "standard", detail: "Confirm that the temporary DSM target session is closed." })
]);
// The second half of the same cross-layer catalogue, for the operations that are
// not Doctor. `src/lib.rs` owns the definition; this is the AppWindow's
// necessarily-duplicated copy, held in step by `test_synology_ui.py` exactly as
// DOCTOR_SECTION_CATALOG above is. Ids, labels, units, table membership and
// order all mirror the Rust tables of the same names.
//
// Read the failure mode before editing either side. The bridge resolves a
// job-supplied phase id against the library table and publishes the label it
// finds; this window renders it, and refuses any label this copy does not carry.
// If the two skew, the phase renders as "progress unavailable" -- loudly, which
// is the point, because the alternative is under-reporting a slow operation at
// exactly the moment someone is watching it to work out why it is slow.
//
// Unlike the doctor sections these tables carry no step: a phase's step is its
// position, because these operations execute and display in the same order.
// Doctor is the exception, which is why its table keeps a step column.
//
// `unit` is part of the catalogue rather than the record's payload, for the same
// reason `label` is: the job supplies one untrusted datum, the phase id, and
// every string that reaches the screen is resolved from it here.
const PHASE_UNITS = Object.freeze(["", "bytes", "entries", "files"]);
// The seven phases of a scoped status query, in the order `run_status` performs
// them. Phases 1, 4 and 5 are the ones that take the minutes an operator is
// staring at; the rest are sub-second and carry no counter.
const SYNC_STATUS_PHASE_SPECS = Object.freeze([
  Object.freeze({ id: "scan_local", label: "Scanning local files", unit: "files" }),
  Object.freeze({ id: "connect", label: "Connecting to DSM", unit: "" }),
  Object.freeze({ id: "authenticate", label: "Authenticating", unit: "" }),
  Object.freeze({ id: "list_remote", label: "Listing remote files", unit: "entries" }),
  Object.freeze({ id: "compare", label: "Comparing file contents", unit: "files" }),
  Object.freeze({ id: "build_report", label: "Building the report", unit: "" }),
  Object.freeze({ id: "store_results", label: "Storing results", unit: "" })
]);
// The six phases a planning run performs before it reports and stops.
const PLAN_PHASE_SPECS = Object.freeze([
  Object.freeze({ id: "scan_local", label: "Scanning local files", unit: "files" }),
  Object.freeze({ id: "connect", label: "Connecting to DSM", unit: "" }),
  Object.freeze({ id: "authenticate", label: "Authenticating", unit: "" }),
  Object.freeze({ id: "list_remote", label: "Listing remote files", unit: "entries" }),
  Object.freeze({ id: "compare", label: "Comparing file contents", unit: "files" }),
  Object.freeze({ id: "build_plan", label: "Building the plan", unit: "" })
]);
// The eight phases of a run that executes its plan. Shared by `run` and `resync`
// because both reach them through the same code path; a resync asked only to
// plan stops at `build_plan` and never reports the last two.
const SYNC_PHASE_SPECS = Object.freeze([
  Object.freeze({ id: "scan_local", label: "Scanning local files", unit: "files" }),
  Object.freeze({ id: "connect", label: "Connecting to DSM", unit: "" }),
  Object.freeze({ id: "authenticate", label: "Authenticating", unit: "" }),
  Object.freeze({ id: "list_remote", label: "Listing remote files", unit: "entries" }),
  Object.freeze({ id: "compare", label: "Comparing file contents", unit: "files" }),
  Object.freeze({ id: "build_plan", label: "Building the plan", unit: "" }),
  Object.freeze({ id: "upload", label: "Uploading files", unit: "files" }),
  Object.freeze({ id: "reconcile", label: "Verifying the result", unit: "" })
]);
// The four phases of a bounded connection probe.
const CONNECTION_PHASE_SPECS = Object.freeze([
  Object.freeze({ id: "resolve_secrets", label: "Reading stored credentials", unit: "" }),
  Object.freeze({ id: "authenticate", label: "Authenticating", unit: "" }),
  Object.freeze({ id: "contact", label: "Contacting File Station", unit: "" }),
  Object.freeze({ id: "logout", label: "Ending the DSM session", unit: "" })
]);
// Keyed by progress catalogue key, deliberately not by queued-mutation operation
// id: three operational actions share the wire id `action` and walk three
// different sequences, so keying on the wire id would resolve a planning run
// against an upload catalogue. `doctor` is absent on purpose and lives in
// DOCTOR_SECTION_CATALOG above.
const PHASE_SPECS = Object.freeze({
  "sync-status": SYNC_STATUS_PHASE_SPECS,
  plan: PLAN_PHASE_SPECS,
  run: SYNC_PHASE_SPECS,
  resync: SYNC_PHASE_SPECS,
  connection: CONNECTION_PHASE_SPECS
});
const DOCTOR_STATE_ALIASES = Object.freeze({
  pass: "ok", passed: "ok", success: "ok", succeeded: "ok", healthy: "ok", ready: "ok", ok: "ok",
  warning: "warn", warned: "warn", degraded: "warn", warn: "warn",
  error: "failed", fail: "failed", failure: "failed", failed: "failed", unhealthy: "failed", not_ok: "failed",
  partial: "warn", preflighted: "warn",
  ignored: "skipped", not_run: "skipped", omitted: "skipped", unsupported: "skipped", not_applicable: "skipped", skip: "skipped", skipped: "skipped",
  queued: "pending", waiting: "pending", pending: "pending", active: "running", executing: "running", running: "running"
});

// How long a progress record may go without being republished before its own
// silence is the more useful fact about the job.
//
// The writer's ceiling is two seconds -- the result poll ramp tops out there, so
// nothing writes more often than that and nobody could observe it if it did --
// and a phase change always writes immediately. A record that has not moved in
// two minutes has missed roughly sixty opportunities to move, which describes a
// wedged job rather than a slow one. Saying so is the point: a progress record
// that stopped updating is information, and it is exactly the information the
// version of this dashboard that rendered nothing at all could not convey.
// Every phase label this AppWindow will ever render, from both halves of the
// catalogue.
//
// The bridge resolves a job-supplied phase id against the library table and
// publishes the label it finds, so a label reaching here has in principle
// already passed an allow-list. This is the same allow-list, enforced again on
// the side that does the rendering, and it buys two things the server-side check
// cannot. A bridge whose table has drifted from this one is caught and reported
// rather than quietly rendering a string this window has never reviewed. And the
// set of text that can reach an operator's screen from a queued job stays finite
// and readable in one place -- which is the property that made progress safe to
// render at all, and is worth not having to take on trust from one layer.
const PROGRESS_LABELS = Object.freeze(
  Object.keys(PHASE_SPECS)
    .reduce((labels, operation) => labels.concat(PHASE_SPECS[operation].map((phase) => phase.label)),
      DOCTOR_SECTION_CATALOG.map((section) => section.label))
);
const PROGRESS_STALE_SECONDS = 120;
// How long the package controller may go without republishing its state before
// a job still sitting in the queue is evidence of a problem rather than of
// ordinary waiting.
//
// The controller rewrites this record on every pass of its loop and its longest
// idle sleep is thirty seconds, so six missed ticks is a daemon that is wedged
// or gone, not one that is busy. Staleness is read only while nothing is
// active, because a controller executing a long job publishes its active PID
// and then blocks for as long as that job takes -- reading staleness there
// would have a four-hour sync report its own controller as dead.
const CONTROLLER_TICK_STALE_SECONDS = 180;
const PROGRESS_UNAVAILABLE_TEXT = "Progress unavailable — the package published a progress record this AppWindow could not validate, so it is not being shown. The operation itself is unaffected and is still queued.";

function formatCount(value) {
  const numeric = Number(value);
  if (!Number.isFinite(numeric) || numeric < 0) return "0";
  try {
    return new Intl.NumberFormat(undefined).format(Math.round(numeric));
  } catch (_error) {
    return String(Math.round(numeric));
  }
}

/**
 * The render-boundary guard for every progress record, whatever door it came in
 * through.
 *
 * Three outcomes, and they are not interchangeable. `null` means no progress was
 * published, and the caller renders nothing extra. A record means it validated.
 * `PROGRESS_UNAVAILABLE` means something arrived and did not validate, and the
 * caller must say so rather than render a blank -- silently dropping a bad
 * record is how a broken writer ships unnoticed, which is the failure this whole
 * change exists to undo.
 *
 * A value still carrying its wire field names never passed the API validator, so
 * it is sent through it here before any part of it is rendered. Every path into
 * this function is supposed to hand over a pre-validated record; this is what
 * makes "supposed to" something we do not have to rely on.
 */
function renderableProgress(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  if (value.unavailable === true) return PROGRESS_UNAVAILABLE;
  const validated = Object.prototype.hasOwnProperty.call(value, "updated_at")
    ? trustedRequestProgress(value)
    : value;
  if (!validated || typeof validated !== "object") return PROGRESS_UNAVAILABLE;
  const step = Number(validated.step);
  const total = Number(validated.total);
  const label = boundedText(validated.label, "").slice(0, 128);
  if (!Number.isInteger(step) || !Number.isInteger(total) || step < 1 || total < 1 || step > total
    || !label || !PROGRESS_LABELS.includes(label)) {
    return PROGRESS_UNAVAILABLE;
  }
  const count = Number(validated.count);
  return {
    step,
    total,
    label,
    unit: PHASE_UNITS.includes(validated.unit) ? validated.unit : "",
    count: Number.isInteger(count) && count >= 0 ? count : 0,
    updatedAt: numberOr(validated.updatedAt, 0)
  };
}

// A running count and its catalogue unit, never a fraction and never a bar.
// The denominator is genuinely unknown while a phase runs -- that is what makes
// the phase indeterminate -- and a percentage computed against a guess is worse
// than no percentage at all on a tool people use to move their own files.
function progressCountText(progress) {
  if (!progress.unit || !progress.count) return "";
  if (progress.unit === "bytes") return `${formatBytes(progress.count)} so far`;
  return `${formatCount(progress.count)} ${progress.unit} so far`;
}

// The package's own clock, for the staleness comparisons that must not be made
// against the browser's. Zero when no snapshot is to hand, which reads as "do
// not claim staleness" rather than as an epoch.
function packageEpoch(component) {
  const liveness = component && component.controllerLiveness;
  return liveness ? numberOr(liveness.packageEpoch, 0) : 0;
}

// The wait explanation is strictly additive: when it cannot be established, the
// queued report still has to render. `reportMutationError` is reached from every
// mutation path in the window, and a missing snapshot must not be able to turn a
// successfully queued change into a thrown error on the way to reporting it.
function queuedWaitText(component) {
  const detail = component && component.queuedWaitDetail;
  return detail && typeof detail.text === "string" ? detail.text : "";
}

// `nowEpoch` is the package's own clock, taken from the snapshot, and there is
// deliberately no fallback to the browser's. Both timestamps being compared are
// written by the NAS, so comparing them is skew-free; comparing one of them
// against the browser would make every record on a NAS whose clock runs a few
// minutes behind look wedged. With no package clock to hand, staleness is simply
// not claimed -- under-reporting a stuck job is recoverable, and crying wolf at
// every operator with an unsynchronised NAS is not.
function progressSentence(value, nowEpoch = 0) {
  const progress = renderableProgress(value);
  if (!progress) return "";
  if (progress === PROGRESS_UNAVAILABLE) return PROGRESS_UNAVAILABLE_TEXT;
  const counted = progressCountText(progress);
  const now = numberOr(nowEpoch, 0);
  const stale = now > 0 && progress.updatedAt > 0 && now - progress.updatedAt >= PROGRESS_STALE_SECONDS;
  return guidanceText(
    `Step ${progress.step} of ${progress.total}: ${progress.label}${counted ? ` — ${counted}` : ""}.`,
    stale
      ? `This has not advanced since ${formatDate(progress.updatedAt)}, so the operation may be stuck; review Logs and Activity.`
      : ""
  );
}

// The named ways a queued job ends without its operation having run, and what a
// person is meant to do about each.
//
// Every one of these used to arrive as `unresolved`: the controller deleted the
// request, wrote its reason to a log nobody was looking at, and the dashboard
// said the outcome could not be established. "Unresolved" with no cause attached
// is the second of the two dumps this change exists to remove, and it is
// indistinguishable from "this request ID was never accepted" -- which calls for
// the opposite action. So each entry carries the cause *and* the next step, and
// neither half is optional: a named failure with no next step is a better error
// message, not a better dashboard.
const QUEUED_FAILURE_COPY = Object.freeze({
  classification_failed: Object.freeze({
    title: "Queued operation was not classified",
    cause: "The package controller could not determine what kind of operation this request was, so it never started it.",
    next: "Nothing ran and nothing was changed. Review the controller log for the classification exit code, then submit the request again."
  }),
  secret_claim_failed: Object.freeze({
    title: "Queued operation could not claim its credential",
    cause: "The stored credential this request needed could not be claimed by the package controller, so the operation was never started.",
    next: "Nothing ran and nothing was changed. Check the profile's stored credential under Security and the controller log, then submit the request again."
  }),
  consumer_failed: Object.freeze({
    title: "Queued operation was terminated",
    cause: "The worker running this operation exited before it finished.",
    next: "On a memory-constrained NAS this is most often the kernel out-of-memory killer. Review Logs, then retry with a narrower scope."
  }),
  consumer_wrote_no_result: Object.freeze({
    title: "Queued operation reported nothing",
    cause: "The worker exited cleanly but wrote no result, so how far it got cannot be established from the queue.",
    next: "Treat this operation as unconfirmed. Review Activity and the current state before submitting it again."
  })
});

function queuedFailureGuidance(error) {
  const copy = error && typeof error.code === "string" ? QUEUED_FAILURE_COPY[error.code] : null;
  if (!copy) return null;
  const exitCode = error && Number.isInteger(error.exitCode) ? error.exitCode : null;
  return {
    title: copy.title,
    text: guidanceText(
      copy.cause,
      exitCode === null ? "" : `The worker exited with code ${exitCode}.`,
      copy.next
    )
  };
}

// What the running operation last told us it was doing. One slot rather than
// one per surface, because the package runs one queued operation at a time and
// the AppWindow blocks the others while it does.
function emptyLiveProgress() {
  return { active: false, operation: "", progress: null };
}

// The observer a queued operation hands to its result poll. Every pending read
// publishes here, so a slow operation can say what it is doing while it runs
// rather than only once the browser's patience has expired -- which was the only
// moment progress had ever been able to reach this window at all.
//
// Marked active at dispatch rather than at the first pending read: the job is
// queued from the moment the package accepts it, and until a phase arrives the
// controller-liveness join is what explains the wait.
//
// A plain function over the component rather than a method on it. This is state
// plumbing with no dispatch of its own, and keeping it off the component means
// driving one of these operations needs only the `liveProgress` slot, not two
// more bindings on whatever context is driving it.
function openProgressSink(component, operation) {
  const name = boundedText(operation, "");
  component.liveProgress = { active: true, operation: name, progress: null };
  return (progress) => {
    if (component.disposed) return;
    component.liveProgress = { active: true, operation: name, progress: progress || null };
  };
}

function closeProgressSink(component) {
  if (!component.disposed) component.liveProgress = emptyLiveProgress();
}

function emptyProfileFailureRecords() {
  return {
    configuration: { active: false, outcomeUnknown: false, requiresInspection: false },
    secrets: {
      password: { active: false, outcomeUnknown: false, requiresInspection: false },
      totp: { active: false, outcomeUnknown: false, requiresInspection: false },
      "remote-log-token": { active: false, outcomeUnknown: false, requiresInspection: false }
    }
  };
}

function emptyProfileCreationProgress() {
  return { active: false, current: 0, total: 0, message: "" };
}

function profileFailureSummary(records) {
  const safe = records && typeof records === "object" ? records : emptyProfileFailureRecords();
  const candidates = [safe.configuration].concat(PROFILE_SECRET_KINDS.map((kind) => safe.secrets && safe.secrets[kind]));
  const active = candidates.filter((record) => record && record.active === true);
  return {
    active: active.length > 0,
    outcomeUnknown: active.some((record) => record.outcomeUnknown === true),
    requiresInspection: active.some((record) => record.requiresInspection === true)
  };
}

function scopeMutationOutcomeUnresolved(component, scope) {
  return Boolean(
    component
    && ((component.autosaveOutcomeUnknownScopes
      && component.autosaveOutcomeUnknownScopes[scope] === true)
      || (component.autosaveInspectionScopes
        && component.autosaveInspectionScopes[scope] === true))
  );
}

function emptyIsolatedIncident() {
  return { active: false, kind: "", operation: "", outcomeUnknown: false, requiresInspection: false, settled: false, message: "", requestId: "", jobId: "", subject: "", retryable: false };
}

// DSM returned a schema-valid terminal result for this exact accepted job, so
// the request outcome is known: it definitively failed. Only the temporary
// File Station session cleanup is in doubt. That is not an unresolved outcome,
// and reconciling it can only re-read the very result already in hand.
function settledTerminalOutcome(error) {
  return Boolean(error
    && error.accepted === true
    && error.outcomeUnknown !== true
    && error.trustedJobId === true
    && validatedJobId(error.jobId));
}

// A scope stays locked only while its outcome is genuinely unknown, or while a
// cleanup failure arrived without a terminal result to attribute it to. A
// settled failure is reported and left unlocked so the operator can correct
// the draft and try again.
function unresolvedIsolatedIncident(incident) {
  return Boolean(incident && incident.active === true
    && (incident.outcomeUnknown === true
      || (incident.requiresInspection === true && incident.settled !== true)));
}

function emptyScopeIncident() {
  return {
    active: false,
    outcomeUnknown: false,
    requiresInspection: false,
    message: "",
    requestId: "",
    jobId: "",
    subject: "",
    operation: "",
    stage: "",
    transportStage: "",
    secretKind: "",
    expectedConfiguration: null,
    creatingProfile: false
  };
}

function isolatedIncidentUnresolved(component, scope) {
  const incident = component && component.isolatedIncidents && component.isolatedIncidents[scope];
  return unresolvedIsolatedIncident(incident);
}

// One walk over every scope that can hold an unresolved incident. The banner's
// summary, its correlation evidence and the set of requests worth probing all
// read from this, so they cannot drift apart about what is actually locked.
function unresolvedIncidentEntries(component) {
  const entries = [];
  for (const scope of AUTOSAVE_SCOPES) {
    if (!scopeMutationOutcomeUnresolved(component, scope)) continue;
    const incident = component.autosaveIncidents && component.autosaveIncidents[scope];
    entries.push({ scope, incident: incident && incident.active === true ? incident : null });
  }
  for (const scope of ["connection", "operations"]) {
    if (isolatedIncidentUnresolved(component, scope)) {
      entries.push({ scope, incident: component.isolatedIncidents[scope] });
    }
  }
  return entries;
}

function unresolvedScopeNames(component) {
  return unresolvedIncidentEntries(component).map(({ scope }) => INCIDENT_SCOPE_LABELS[scope]);
}

function hasAnyUnresolvedIncident(component) {
  return unresolvedScopeNames(component).length > 0;
}

// The one place any incident turns into correlation evidence. Five copies of
// this list had drifted apart only in how they label the subject, which is the
// single thing a caller actually varies.
function incidentCorrelation(incident, subjectPrefix = "Subject:") {
  return incident
    ? [
      incident.subject ? `${subjectPrefix} ${incident.subject}.` : "",
      incident.requestId ? `Client request ID: ${incident.requestId}.` : "",
      incident.jobId ? `Queued job ID: ${incident.jobId}.` : ""
    ].filter(Boolean).join(" ")
    : "";
}

function guidanceText(...parts) {
  return parts.filter(Boolean).join(" ").replace(/\s+/g, " ").trim();
}

function scopeMutationGuidance(component, scope) {
  const unknown = Boolean(component && component.autosaveOutcomeUnknownScopes
    && component.autosaveOutcomeUnknownScopes[scope] === true);
  const incident = component && component.autosaveIncidents && component.autosaveIncidents[scope];
  return guidanceText(
    `${INCIDENT_SCOPE_LABELS[scope] || scope} is locked in this AppWindow.`,
    unknown
      ? "Its previous request may already have been accepted."
      : "Its previous request was only partially applied and needs inspection.",
    incidentCorrelation(incident && incident.active === true ? incident : null),
    "Preserve the draft and inspect Activity / Logs plus current package state before reconciling it.",
    scope === "profile"
      ? "Independent configuration scopes and their autosave remain available; routine changes and Run / Doctor stay paused because they depend on settled profile state."
      : "Unrelated controls and autosave remain available."
  );
}

function isolatedIncidentGuidance(component, scope) {
  const incident = component && component.isolatedIncidents && component.isolatedIncidents[scope];
  return guidanceText(
    `${INCIDENT_SCOPE_LABELS[scope] || scope} is locked in this AppWindow.`,
    incident && incident.outcomeUnknown === true
      ? "The previous request may already have been accepted."
      : "The previous request or cleanup needs inspection.",
    incidentCorrelation(incident),
    scope === "connection"
      ? "Resolve the exact request before changing the preserved profile and credential draft or starting another authentication or File Station request. Unrelated controls and autosave remain available."
      : "Inspect Activity / Logs and current package state before another request in this scope. Profile saves and unrelated controls and autosave remain available."
  );
}

function unresolvedIncidentGuidance(component) {
  const scopes = unresolvedScopeNames(component);
  if (!scopes.length) return "No unresolved operation outcomes.";
  const listed = scopes.length === 1 ? scopes[0] : `${scopes.slice(0, -1).join(", ")} and ${scopes[scopes.length - 1]}`;
  const evidence = unresolvedIncidentEntries(component)
    .map(({ scope, incident }) => incidentCorrelation(incident, `${INCIDENT_SCOPE_LABELS[scope]} subject:`));
  // What is blocked and what is not is now stated explicitly beside this, from
  // the same predicates the controls consult. Repeating it as prose here only
  // gave the operator two claims to reconcile against each other.
  return guidanceText(
    `${listed} ${scopes.length === 1 ? "needs" : "need"} reconciliation.`,
    evidence.filter(Boolean).join(" "),
    "The package is being asked what became of the exact request; preserve any open draft while that settles."
  );
}

// Every unresolved incident that names an exact request the package can be asked
// about. The manual Reconcile controls are restricted to the four operations that
// have an apply path; this is deliberately not, because establishing what happened
// applies nothing and is useful for every locked scope.
function probeableIncidents(component) {
  return unresolvedIncidentEntries(component)
    .filter(({ incident }) => incident && validatedClientRequestId(incident.requestId) && incident.operation);
}

// What the app has established so far, in the operator's terms. `absent` is a
// finding and says so; only `unavailable` sends anyone to the logs.
//
// `absent` deliberately does not read as "so it never happened". No record can
// mean the request never reached DSM, or that its completed job has already been
// reaped, and those two differ in exactly the way that matters before a retry.
const PROBE_VERDICT_COPY = Object.freeze({
  settled: "DSM holds a completed record for this exact request.",
  accepted: "DSM accepted this request and its job is still running.",
  unknown: "The package recorded this request's own outcome as unknown.",
  absent: "Neither the package queue nor Activity holds any record of this request ID. That does not confirm it never ran: a completed job's record is eventually reaped, so this stays locked.",
  unavailable: "This check could not reach a trustworthy answer."
});
// Scopes whose lock exists only because the outcome was unknown. They have no
// draft to reapply and no snapshot to match, so once the package proves what
// became of the exact request, the premise of the lock is gone and it is
// released. Profile and connection are absent on purpose: they have an apply
// path, and it stays on the reviewed manual Reconcile controls.
const SELF_RELEASING_INCIDENT_SCOPES = Object.freeze(["routine", "alerts", "security", "interface", "operations"]);
// Cheap polling on a CPU-constrained NAS: start responsive, decay to a minute.
const INCIDENT_PROBE_RAMP_MS = Object.freeze([2000, 5000, 10000, 20000, 30000, 60000]);

// What is actually gated, derived from the same unresolved-scope predicates the
// controls themselves consult, so the banner cannot claim availability the gates
// do not grant. The prose it replaces asserted "unrelated controls remain
// available" and was simply not believed.
const INCIDENT_GATES = Object.freeze([
  Object.freeze([INCIDENT_SCOPE_LABELS.profile, Object.freeze(["profile", "connection"])]),
  Object.freeze([INCIDENT_SCOPE_LABELS.connection, Object.freeze(["profile", "connection"])]),
  Object.freeze([INCIDENT_SCOPE_LABELS.routine, Object.freeze(["profile", "routine"])]),
  Object.freeze([INCIDENT_SCOPE_LABELS.operations, Object.freeze(["profile", "operations"])]),
  Object.freeze([INCIDENT_SCOPE_LABELS.alerts, Object.freeze(["alerts"])]),
  Object.freeze([INCIDENT_SCOPE_LABELS.security, Object.freeze(["security"])]),
  Object.freeze([INCIDENT_SCOPE_LABELS.interface, Object.freeze(["interface"])])
]);

function emptyIncidentProbe() {
  return { active: false, scope: "", verdict: "", attempts: 0, checkedAt: 0, jobId: "", progress: null, message: "" };
}

function unresolvedScopeError(component, scope) {
  const error = new Error(scopeMutationGuidance(component, scope));
  error.outcomeUnknown = true;
  error.requiresInspection = true;
  return error;
}

function recordIsolatedIncident(component, scope, kind, error, report = null, metadata = undefined) {
  const outcomeUnknown = Boolean(report ? report.unknown === true : error && error.outcomeUnknown === true);
  const requiresInspection = Boolean(
    outcomeUnknown
    || (report ? report.inspection === true : error && error.requiresInspection === true)
  );
  if (!outcomeUnknown && !requiresInspection) return false;
  const incidents = component.isolatedIncidents && typeof component.isolatedIncidents === "object"
    ? component.isolatedIncidents
    : { connection: emptyIsolatedIncident(), operations: emptyIsolatedIncident() };
  const previous = incidents[scope] || emptyIsolatedIncident();
  const details = metadata && typeof metadata === "object" ? metadata : {};
  // Preserving earlier evidence exists to stop later noise from overwriting an
  // outcome nobody has resolved yet. A settled failure is already resolved, so
  // it must not shadow the evidence for the request the operator just made.
  const preserve = unresolvedIsolatedIncident(previous);
  const settled = settledTerminalOutcome(error);
  incidents[scope] = {
    active: true,
    kind: preserve ? previous.kind : kind,
    operation: preserve
      ? previous.operation
      : boundedText((error && error.operation) || details.operation, "").slice(0, 64),
    outcomeUnknown: previous.outcomeUnknown === true || outcomeUnknown,
    requiresInspection: previous.requiresInspection === true || requiresInspection,
    settled: preserve ? previous.settled === true : settled,
    message: preserve ? previous.message : boundedText((report && report.message) || (error && error.message), "Operation evidence needs inspection.").slice(0, MUTATION_MESSAGE_LIMIT),
    requestId: preserve ? previous.requestId : ((report && report.requestId) || ""),
    jobId: preserve ? previous.jobId : ((report && report.jobId) || ""),
    subject: preserve ? previous.subject : boundedText(details.subject, "").slice(0, 256),
    retryable: false
  };
  component.isolatedIncidents = incidents;
  return true;
}

function clearIsolatedIncident(component, scope) {
  if (!component.isolatedIncidents || typeof component.isolatedIncidents !== "object") return;
  component.isolatedIncidents[scope] = emptyIsolatedIncident();
}

function recordScopeIncident(component, scope, error, subject = "", metadata = undefined) {
  const outcomeUnknown = Boolean(error && error.outcomeUnknown === true);
  const requiresInspection = Boolean(error && (error.requiresInspection === true || outcomeUnknown));
  if (!outcomeUnknown && !requiresInspection) return false;
  if (!component.autosaveIncidents || typeof component.autosaveIncidents !== "object") {
    component.autosaveIncidents = Object.fromEntries(AUTOSAVE_SCOPES.map((name) => [name, emptyScopeIncident()]));
  }
  const previous = component.autosaveIncidents[scope] || emptyScopeIncident();
  if (previous.active === true) return true;
  const details = metadata && typeof metadata === "object" ? metadata : {};
  const operation = boundedText(error && error.operation, "").slice(0, 64);
  const transportStage = boundedText(error && error.stage, "").slice(0, 128);
  const secretKind = PROFILE_SECRET_KINDS.includes(details.secretKind) ? details.secretKind : "";
  const stage = secretKind ? `secret:${secretKind}` : (operation === ACTIONS.configureProfile ? "configuration" : transportStage);
  component.autosaveIncidents[scope] = {
    active: true,
    outcomeUnknown,
    requiresInspection,
    message: boundedText(error && error.message, "Mutation evidence needs inspection.").slice(0, MUTATION_MESSAGE_LIMIT),
    requestId: error && error.trustedRequestId === true ? validatedClientRequestId(error.requestId) : "",
    jobId: error && error.trustedJobId === true ? validatedJobId(error.jobId) : "",
    subject: boundedText(subject, "").slice(0, 256),
    operation,
    stage,
    transportStage,
    secretKind,
    expectedConfiguration: details.expectedConfiguration && typeof details.expectedConfiguration === "object"
      ? JSON.parse(JSON.stringify(details.expectedConfiguration))
      : null,
    creatingProfile: details.creatingProfile === true
  };
  return true;
}

function clearScopeIncident(component, scope) {
  if (!component.autosaveIncidents || typeof component.autosaveIncidents !== "object") return;
  component.autosaveIncidents[scope] = emptyScopeIncident();
}

function currentAutosaveStatus(
  coordinator,
  failures,
  savedMessage = "All changes saved",
  outcomeUnknownScopes = null,
  inspectionScopes = null,
  heldScopes = null
) {
  for (const scope of AUTOSAVE_SCOPES) {
    if (failures && failures[scope] === true && outcomeUnknownScopes && outcomeUnknownScopes[scope] === true) {
      return {
        phase: "blocked",
        message: `${scope.charAt(0).toUpperCase()}${scope.slice(1)} autosave outcome unknown · inspect Activity / Logs before any retry`
      };
    }
  }
  for (const scope of AUTOSAVE_SCOPES) {
    if (failures && failures[scope] === true && inspectionScopes && inspectionScopes[scope] === true) {
      return {
        phase: "blocked",
        message: `${scope.charAt(0).toUpperCase()}${scope.slice(1)} changes need inspection · inspect Activity / Logs before another mutation`
      };
    }
  }
  for (const scope of AUTOSAVE_SCOPES) {
    if (failures && failures[scope] === true) {
      return { phase: "blocked", message: `${scope.charAt(0).toUpperCase()}${scope.slice(1)} autosave paused · use Save now` };
    }
  }
  if (!coordinator) return { phase: "saved", message: savedMessage };
  const registered = AUTOSAVE_SCOPES.map((scope) => coordinator.getState(scope)).filter((state) => state.registered);
  const states = registered.filter((state) => !state.cancelled);
  // A scope held while a connection request is in flight is blocked on purpose
  // and drains itself when the request settles. The generic blocked message
  // below would tell the operator to press Save now, which is work they do not
  // have to do and which they would be doing to recover an edit that is not
  // actually at risk.
  const isHeld = (state) => Boolean(heldScopes && heldScopes[state.scope] === true);
  if (states.some((state) => state.dirty && isHeld(state))) {
    return { phase: "pending", message: "Autosave held until the connection request settles" };
  }
  if (states.some((state) => state.blocked && state.dirty)) return { phase: "blocked", message: "Changes require Save now" };
  if (states.some((state) => state.inFlight)) return { phase: "saving", message: "Saving changes…" };
  if (states.some((state) => state.dirty || state.scheduled || state.queued)) return { phase: "pending", message: "Autosave pending · 1.3 seconds" };
  // Never report "saved" over an entry that still holds an unsaved edit.
  //
  // A cancelled scope is excluded from every phase above because nothing will
  // dispatch it -- which is exactly why it must not then fall through to
  // "saved". That combination is what made the connection-hold defect silent:
  // the edit was stranded *and* the status line said it had been saved, so
  // there was nothing for the operator to notice. Any future caller that
  // cancels a scope the user is still editing now shows up here instead.
  if (registered.some((state) => state.dirty)) return { phase: "blocked", message: "Unsaved changes · use Save now" };
  return { phase: "saved", message: savedMessage };
}

const APP_CLASS = "SYNO.SDS.App.SynologyDriveSync.Instance";
const HELP_APPLICATION = "SYNO.SDS.HelpBrowser.Application";
const HELP_CONTENT = Object.freeze({
  overview: "overview.html",
  profiles: "profiles.html",
  routines: "routines.html",
  sync: "sync.html",
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
  licenseUrl: "https://github.com/supermarsx/synology-drive-sync/blob/main/license.md",
  coreVersion: "0.1.0",
  uiVersion: "1.0.0",
  apiSchema: SNAPSHOT_SCHEMA,
  releasesUrl: "https://github.com/supermarsx/synology-drive-sync/releases",
  releaseSelectorUrl: "https://supermarsx.github.io/synology-drive-sync/release-selector.html"
});
const ABOUT_RUST_DEPENDENCIES = Object.freeze([
  { name: "clap", pin: "4.6.6", scope: "All platforms", url: "https://crates.io/crates/clap" },
  { name: "clap_complete", pin: "4.6.9", scope: "All platforms", url: "https://crates.io/crates/clap_complete" },
  { name: "clap_mangen", pin: "0.3.3", scope: "All platforms", url: "https://crates.io/crates/clap_mangen" },
  { name: "crc32fast", pin: "1.5.1", scope: "All platforms", url: "https://crates.io/crates/crc32fast" },
  { name: "ctrlc", pin: "3.5.2", scope: "All platforms", url: "https://crates.io/crates/ctrlc" },
  { name: "ignore", pin: "0.4.33", scope: "All platforms", url: "https://crates.io/crates/ignore" },
  { name: "keyring-core", pin: "1.0.0", scope: "All platforms", url: "https://crates.io/crates/keyring-core" },
  { name: "md-5", pin: "0.11.0", scope: "All platforms", url: "https://crates.io/crates/md-5" },
  { name: "hmac", pin: "0.13.0", scope: "All platforms", url: "https://crates.io/crates/hmac" },
  { name: "reqwest", pin: "0.13.4", scope: "All platforms", url: "https://crates.io/crates/reqwest" },
  { name: "rustls", pin: "0.23.43", scope: "All platforms", url: "https://crates.io/crates/rustls" },
  { name: "rpassword", pin: "7.5.4", scope: "All platforms", url: "https://crates.io/crates/rpassword" },
  { name: "serde", pin: "1.0.229", scope: "All platforms", url: "https://crates.io/crates/serde" },
  { name: "serde_json", pin: "1.0.151", scope: "All platforms", url: "https://crates.io/crates/serde_json" },
  { name: "sha2", pin: "0.11.0", scope: "All platforms", url: "https://crates.io/crates/sha2" },
  { name: "subtle", pin: "2.6.1", scope: "All platforms", url: "https://crates.io/crates/subtle" },
  { name: "thiserror", pin: "2.0.20", scope: "All platforms", url: "https://crates.io/crates/thiserror" },
  { name: "tokio", pin: "1.53.1", scope: "All platforms", url: "https://crates.io/crates/tokio" },
  { name: "toml", pin: "1.1.4+spec-1.1.0", scope: "All platforms", url: "https://crates.io/crates/toml" },
  { name: "totp-rs", pin: "6.0.0", scope: "All platforms", url: "https://crates.io/crates/totp-rs" },
  { name: "zeroize", pin: "1.9.0", scope: "All platforms", url: "https://crates.io/crates/zeroize" },
  { name: "windows-native-keyring-store", pin: "1.1.0", scope: "Windows", url: "https://crates.io/crates/windows-native-keyring-store" },
  { name: "apple-native-keyring-store", pin: "1.0.2", scope: "macOS", url: "https://crates.io/crates/apple-native-keyring-store" },
  { name: "libc", pin: "0.2.189", scope: "Linux", url: "https://crates.io/crates/libc" },
  { name: "zbus-secret-service-keyring-store", pin: "1.0.1", scope: "Linux", url: "https://crates.io/crates/zbus-secret-service-keyring-store" }
]);
const ABOUT_UI_DEPENDENCIES = Object.freeze([
  { name: "@babel/core", pin: "8.0.1", scope: "devDependency", url: "https://www.npmjs.com/package/@babel/core" },
  { name: "@babel/preset-env", pin: "8.0.2", scope: "devDependency", url: "https://www.npmjs.com/package/@babel/preset-env" },
  { name: "babel-loader", pin: "10.1.1", scope: "devDependency", url: "https://www.npmjs.com/package/babel-loader" },
  { name: "css-loader", pin: "7.1.5", scope: "devDependency", url: "https://www.npmjs.com/package/css-loader" },
  { name: "mini-css-extract-plugin", pin: "2.10.2", scope: "devDependency", url: "https://www.npmjs.com/package/mini-css-extract-plugin" },
  { name: "vue", pin: "2.7.16", scope: "devDependency", url: "https://www.npmjs.com/package/vue" },
  { name: "vue-loader", pin: "15.11.1", scope: "devDependency", url: "https://www.npmjs.com/package/vue-loader" },
  { name: "vue-template-compiler", pin: "2.7.16", scope: "devDependency", url: "https://www.npmjs.com/package/vue-template-compiler" },
  { name: "webpack", pin: "5.110.3", scope: "devDependency", url: "https://www.npmjs.com/package/webpack" },
  { name: "webpack-cli", pin: "7.2.3", scope: "devDependency", url: "https://www.npmjs.com/package/webpack-cli" },
  { name: "pnpm", pin: "pnpm@11.25.0", scope: "packageManager", url: "https://pnpm.io/" }
]);
const CONTROL_HELP = Object.freeze({
  "profile-filter": "Search configured profiles by name, local source, File Station URL, DSM account, or destination path.",
  "profile-filter-status": "Limit the catalog to ready, missing-password, default, or automated profiles.",
  "profile-name": "Stable profile identifier; existing names cannot be changed.",
  "profile-source": "Absolute /volumeN source. Browse NAS shows only folders the package account can read and traverse; manual entry is still validated by the controller before save.",
  "profile-url": "HTTPS origin of the destination NAS File Station API.",
  "profile-username": "Destination DSM account used only by this profile.",
  "profile-remote": "Logical File Station destination path, not a local mount path. Test authentication before using the ACL-aware target browser.",
  "profile-compare": "Evidence used to decide whether a destination file needs upload.",
  "profile-jobs": "Parallel upload workers; accepted range is 1 through 16.",
  "profile-http": "Permit unencrypted HTTP only for an explicitly controlled LAN.",
  "profile-delete": "Allow deletion only when both the saved profile and a run approve it.",
  "profile-max-delete": "Hard per-profile ceiling that stops excessive destination deletion. New profiles default to 100.",
  "profile-excludes": "Enter one package-relative exclusion pattern per line. New profiles start with the four DSM-safe defaults; remove every line only to clear them explicitly.",
  "profile-empty-source": "Disable the empty-source guard only when an empty source is intentional. This exception requires bounded destination deletion to be enabled.",
  "profile-retries": "Retry count for transient upload failures; accepted range is 0 through 5.",
  "profile-timeout": "Maximum time allowed for one upload request.",
  "profile-connect-timeout": "Maximum time allowed to establish the destination connection.",
  "profile-rate": "Upload bandwidth ceiling in bytes per second; zero means unlimited.",
  "profile-ca": "Absolute NAS path to a trusted PEM certificate bundle.",
  "profile-invalid-certs": "Bypass TLS certificate validation; this exposes credentials to interception.",
  "profile-invalid-confirm": "Required acknowledgement before saving the unsafe TLS override.",
  "profile-verbosity": "Choose the amount of operational detail written to logs.",
  "profile-quiet": "Suppress the interactive terminal sink while retaining durable package logs. It may be combined with higher verbosity for richer durable records.",
  "profile-log-level": "Minimum severity retained by package logging.",
  "profile-log-format": "Choose human-readable or structured JSON records for this profile.",
  "profile-progress": "Choose automatic, always-on, or disabled progress rendering for this profile.",
  "profile-output": "Choose human-readable, JSON, or newline-delimited JSON command output for this profile.",
  "profile-log-file": "Fixed package-owned sync log path. Profiles cannot redirect it outside protected package storage.",
  "profile-log-url": "Optional HTTPS endpoint for bounded structured events. Removing it disables delivery without clearing the stored token.",
  "profile-log-mode": "Choose whether remote-log delivery failure can fail a sync.",
  "secret-log-mode": "Keep, replace, or clear the package-protected collector token independently. Delivery uses it only while an HTTPS remote-log URL is configured.",
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
  "routine-backoff": "Base retry delay from 10 through 300 seconds; every computed delay is capped at 300 seconds.",
  "routine-exponential": "Increase the retry delay after each failure; fixed and exponential delays never exceed 300 seconds.",
  "routine-dependencies": "Require selected profile routines to finish before this routine starts.",
  "routine-weekdays": "Weekdays on which this routine may execute.",
  "routine-delete": "Permit this routine to use the profile's separately approved deletion policy.",
  "routine-max-delete": "Additional routine-level ceiling for destination deletions.",
  "sync-profile": "Source folder and File Station destination to compare. One profile at a time.",
  "sync-scope": "One source-relative folder or file. Empty examines the whole tree. A path, not a pattern.",
  "sync-filter": "List only file names containing this text, ignoring case. It narrows the rows, never the totals.",
  "sync-state": "Needs attention covers the four states requiring a person: type conflicts, missing remotely, differs, remote only.",
  "sync-limit": "Rows per page. The package returns at most 200 at once, so a large scope is read a page at a time.",
  "sync-excluded": "Also walk entries excluded by ignore rules or DSM-managed paths. The query then examines more of the tree.",
  "resync-scope": "Folder or file to re-upload. Empty re-uploads the whole profile. Overwriting never deletes.",
  "doctor-scope": "Run diagnostics for every profile or one selected profile.",
  "doctor-level": "Quick performs unauthenticated routing, TLS, and DSM API discovery only. Standard and Extensive authenticate and perform bounded inventory. With a configured destination, they check permission and sample its direct children. Without one, they skip permission and sample visible shared-folder roots without selecting or traversing a share. Extensive deepens the read-only checks.",
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
  "settings-status-refresh": "Cadence for authenticated package status refreshes while visible; Manual disables its timer.",
  "settings-log-refresh": "Cadence for live log refreshes while the Activity page is visible; Manual disables its timer."
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
      status_refresh: [0, 1000, 3000, 5000, 10000, 30000].includes(Number(parsed.status_refresh)) ? Number(parsed.status_refresh) : fallback.status_refresh,
      log_refresh: [0, 5000, 10000, 30000].includes(Number(parsed.log_refresh)) ? Number(parsed.log_refresh) : fallback.log_refresh,
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

function utf8ByteLength(value) {
  let bytes = 0;
  for (const character of String(value || "")) {
    const point = character.codePointAt(0);
    bytes += point <= 0x7f ? 1 : (point <= 0x7ff ? 2 : (point <= 0xffff ? 3 : 4));
  }
  return bytes;
}

function hasControlCharacter(value) {
  return Array.from(String(value || "")).some((character) => {
    const point = character.codePointAt(0);
    return point <= 0x1f || (point >= 0x7f && point <= 0x9f);
  });
}

function validBoundedText(value, maximumBytes) {
  return typeof value === "string"
    && value.length > 0
    && utf8ByteLength(value) <= maximumBytes
    && !hasControlCharacter(value);
}

function hasDotPathSegment(value) {
  return String(value || "").split("/").some((segment) => segment === "." || segment === "..");
}

function validLocalSourcePath(value) {
  if (!validBoundedText(value, 4096) || value === "/" || !value.startsWith("/")
    || value.endsWith("/") || value.includes("//") || value.includes("\\") || value.includes('"')) return false;
  const components = value.slice(1).split("/");
  if (!/^volume(?:USB|SATA)?[1-9][0-9]*$/.test(components[0])) return false;
  const managed = new Set(["#recycle", "#snapshot", "@eadir", "@tmp", "@sharebin", "@apphome", "@appdata", "@appstore", "@apptemp", "@appconf", ".synologyworkingdirectory"]);
  return components.every((component) => component && component !== "." && component !== ".." && !managed.has(component.toLowerCase()));
}

function emptyProfile() {
  return {
    name: "", source: "", url: "", username: "", remote: "", compare: "content", jobs: 2,
    allow_http: false, delete: false, max_delete: 100, make_default: false,
    excludes: "@eaDir/\n**/@eaDir/\n#recycle/\n#snapshot/",
    allow_empty_source: false, retries: 2, timeout: 7200, connect_timeout: 15, max_rate: 0,
    ca_certificate: "", danger_invalid_certs: false, danger_invalid_confirm: false,
    verbosity: 0, quiet: false, log_level: "info", log_format: "json", progress: "never",
    output: "human", remote_log_url: "", remote_log_mode: "best-effort"
  };
}

function emptyPathBrowser() {
  return { visible: false, kind: "", current: "/", parent: null, directories: [], loading: false, error: "", truncated: false, request: 0 };
}

function emptyRoutine(profile = "") {
  return { profile, enabled: false, action: "sync", mode: "interval", interval_seconds: 3600, weekdays: [1, 2, 3, 4, 5, 6, 7], time_window_start: "00:00", time_window_end: "23:59", debounce_seconds: 45, poll_seconds: 30, retry_count: 5, retry_backoff_seconds: 60, retry_exponential: true, allow_delete: false, max_total_delete: 100, depends_on: [] };
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
const TROUBLESHOOTING_RECORD_LIMIT = 64 * 1024;
const TROUBLESHOOTING_VISIBLE_LIMIT = 256 * 1024;
const TROUBLESHOOTING_CREDENTIAL_KEY_SOURCE = [
  "password", "passwd", "passphrase", "secret[_-]?value", "totp[_-]?secret", "secret", "totp", "otp[_-]?code",
  "proxy[_-]?authorization", "authorization",
  "x[_-]?sdsync[_-]?csrf", "connection[_-]?proof", "csrf[_-]?(?:header|key|token)", "csrf",
  "sdsync[_-]?(?:password|otp|totp|remote[_-]?log[_-]?token)",
  "http[_-]?(?:authorization|proxy[_-]?authorization|cookie|x[_-]?syno[_-]?token|x[_-]?sdsync[_-]?csrf)",
  "syno[_-]?token", "token",
  "access[_-]?token", "refresh[_-]?token", "session[_-]?(?:id|token)",
  "api[_-]?(?:key|token)", "remote[_-]?log[_-]?token",
  "x[_-]?syno[_-]?(?:token|sid)", "x[_-]?api[_-]?key",
  "_?s{1,2}id", "cookie", "set[_-]?cookie"
].join("|");
const TROUBLESHOOTING_BARE_ENCODED_QUOTE_SOURCE = String.raw`%(?:25){0,2}(?:22|27)`;
// Percent-encoded structural boundaries mirror the raw `[^a-z0-9_-]`
// boundary without accidentally treating encoded letters, digits, `_`, or
// `-` as separators inside a longer metadata key.
const TROUBLESHOOTING_ENCODED_BOUNDARY_SOURCE = String.raw`%(?:25){0,2}(?:0[0-9a-f]|1[0-9a-f]|2[0-9a-c]|2[e-f]|3[a-f]|40|5[b-e]|60|7[b-f])`;
const TROUBLESHOOTING_WRAPPED_ENCODED_QUOTE_SOURCE = [
  String.raw`(?:(?:%5c)(?:(?:%5c){2})*)?%(?:22|27)`,
  String.raw`(?:(?:%255c)(?:(?:%255c){2})*)?%(?:2522|2527)`,
  String.raw`(?:(?:%25255c)(?:(?:%25255c){2})*)?%(?:252522|252527)`
].join("|");
const TROUBLESHOOTING_CREDENTIAL_FIELD_PATTERN = new RegExp(
  // Start at the final opening quote itself. Consuming an arbitrary encoded
  // backslash run in this unanchored prefix makes a no-quote `%5c` run
  // quadratic; wrapper consumption is needed only after an exact key match.
  String.raw`((?:^|(?:${TROUBLESHOOTING_BARE_ENCODED_QUOTE_SOURCE})|(?:${TROUBLESHOOTING_ENCODED_BOUNDARY_SOURCE})|[^a-z0-9_-])(?:${TROUBLESHOOTING_CREDENTIAL_KEY_SOURCE})(?:\\*["']|(?:${TROUBLESHOOTING_WRAPPED_ENCODED_QUOTE_SOURCE}))?[ \t]*(?::|=|%(?:25){0,2}(?:3a|3d))[ \t]*)`,
  "gim"
);
const TROUBLESHOOTING_NEXT_FIELD_PATTERN = new RegExp(
  String.raw`[ \t]*(?:(?:\\*["'])|(?:${TROUBLESHOOTING_WRAPPED_ENCODED_QUOTE_SOURCE}))?[a-z_][a-z0-9_.-]*(?:(?:\\*["'])|(?:${TROUBLESHOOTING_WRAPPED_ENCODED_QUOTE_SOURCE}))?[ \t]*(?::|=|%(?:25){0,2}(?:3a|3d))[ \t]*`,
  "iy"
);
const TROUBLESHOOTING_ENCODED_QUOTE_LEVELS = Object.freeze([
  Object.freeze({ backslash: "%5c", quotes: Object.freeze(["%22", "%27"]) }),
  Object.freeze({ backslash: "%255c", quotes: Object.freeze(["%2522", "%2527"]) }),
  Object.freeze({ backslash: "%25255c", quotes: Object.freeze(["%252522", "%252527"]) })
]);
const TROUBLESHOOTING_URL_COLON_TOKENS = Object.freeze(["%25253a", "%253a", "%3a", ":"]);
const TROUBLESHOOTING_URL_SLASH_TOKENS = Object.freeze(["%25252f", "%252f", "%2f", "/"]);
const TROUBLESHOOTING_URL_AT_TOKENS = Object.freeze(["%252540", "%2540", "%40", "@"]);
const TROUBLESHOOTING_COOKIE_EQUALS_TOKENS = Object.freeze(["%25253d", "%253d", "%3d", "="]);
const TROUBLESHOOTING_COOKIE_SEMICOLON_TOKENS = Object.freeze(["%25253b", "%253b", "%3b", ";"]);
const TROUBLESHOOTING_COOKIE_SPACE_TOKENS = Object.freeze(["%252520", "%2520", "%20"]);

function troubleshootingBackslashCountBefore(value, offset) {
  let count = 0;
  for (let index = offset - 1; index >= 0 && value[index] === "\\"; index -= 1) count += 1;
  return count;
}

function troubleshootingTokenLengthAt(lowercaseValue, offset, tokens) {
  for (const token of tokens) {
    if (lowercaseValue.startsWith(token, offset)) return token.length;
  }
  return 0;
}

function troubleshootingLineEnd(value, start, lineState) {
  if (lineState.end === value.length || start < lineState.end) return lineState.end;
  const lineBreak = value.indexOf("\n", start);
  lineState.end = lineBreak < 0 ? value.length : lineBreak;
  return lineState.end;
}

function troubleshootingNextFieldStartsAt(value, start) {
  TROUBLESHOOTING_NEXT_FIELD_PATTERN.lastIndex = start;
  const match = TROUBLESHOOTING_NEXT_FIELD_PATTERN.exec(value);
  TROUBLESHOOTING_NEXT_FIELD_PATTERN.lastIndex = 0;
  return Boolean(match);
}

function troubleshootingCredentialValueEnd(value, lowercaseValue, start, lineState) {
  let contentStart = start;
  while (contentStart < value.length) {
    const character = value.charCodeAt(contentStart);
    if (character !== 9 && character !== 10 && character !== 13 && character !== 32) break;
    contentStart += 1;
  }
  let lineEnd = troubleshootingLineEnd(value, contentStart, lineState);
  let rawQuoteStart = contentStart;
  while (value[rawQuoteStart] === "\\") rawQuoteStart += 1;
  const rawQuote = value[rawQuoteStart];
  if (rawQuote === "\"" || rawQuote === "'") {
    const openingBackslashes = rawQuoteStart - contentStart;
    for (let index = rawQuoteStart + 1; index < lineEnd; index += 1) {
      if (value[index] !== rawQuote) continue;
      const precedingBackslashes = troubleshootingBackslashCountBefore(value, index);
      const closesValue = openingBackslashes === 0
        ? precedingBackslashes % 2 === 0
        : precedingBackslashes === openingBackslashes;
      if (closesValue) return index + 1;
    }
    return lineEnd;
  }

  for (const level of TROUBLESHOOTING_ENCODED_QUOTE_LEVELS) {
    let quoteStart = contentStart;
    let openingBackslashes = 0;
    while (lowercaseValue.startsWith(level.backslash, quoteStart)) {
      openingBackslashes += 1;
      quoteStart += level.backslash.length;
    }
    const delimiter = level.quotes.find((candidate) => lowercaseValue.startsWith(candidate, quoteStart));
    if (!delimiter) continue;
    for (let closing = lowercaseValue.indexOf(delimiter, quoteStart + delimiter.length);
      closing >= 0 && closing < lineEnd;
      closing = lowercaseValue.indexOf(delimiter, closing + delimiter.length)) {
      let precedingBackslashes = 0;
      for (let cursor = closing - level.backslash.length;
        cursor >= contentStart && lowercaseValue.startsWith(level.backslash, cursor);
        cursor -= level.backslash.length) precedingBackslashes += 1;
      const closesValue = openingBackslashes === 0
        ? precedingBackslashes % 2 === 0
        : precedingBackslashes === openingBackslashes;
      if (closesValue) return closing + delimiter.length;
    }
    return lineEnd;
  }

  while (lineEnd < value.length) {
    let nextLineStart = lineEnd + 1;
    while (value[nextLineStart] === " " || value[nextLineStart] === "\t") nextLineStart += 1;
    if (troubleshootingNextFieldStartsAt(value, nextLineStart)) return lineEnd;
    lineEnd = troubleshootingLineEnd(value, nextLineStart, lineState);
  }
  return lineEnd;
}

function redactTroubleshootingCredentialFields(value) {
  const lowercaseValue = value.toLowerCase();
  const output = [];
  let cursor = 0;
  const lineState = { end: -1 };
  TROUBLESHOOTING_CREDENTIAL_FIELD_PATTERN.lastIndex = 0;
  for (let match = TROUBLESHOOTING_CREDENTIAL_FIELD_PATTERN.exec(value); match; match = TROUBLESHOOTING_CREDENTIAL_FIELD_PATTERN.exec(value)) {
    const valueStart = TROUBLESHOOTING_CREDENTIAL_FIELD_PATTERN.lastIndex;
    const valueEnd = troubleshootingCredentialValueEnd(value, lowercaseValue, valueStart, lineState);
    output.push(value.slice(cursor, valueStart), "[redacted]");
    cursor = valueEnd;
    TROUBLESHOOTING_CREDENTIAL_FIELD_PATTERN.lastIndex = valueEnd;
  }
  TROUBLESHOOTING_CREDENTIAL_FIELD_PATTERN.lastIndex = 0;
  output.push(value.slice(cursor));
  return output.join("");
}

function troubleshootingUrlPrefixEnd(lowercaseValue, start) {
  let cursor = start;
  let schemeLength = 0;
  for (const scheme of ["https", "http", "ftp"]) {
    if (lowercaseValue.startsWith(scheme, start)) {
      schemeLength = scheme.length;
      break;
    }
  }
  if (schemeLength > 0 && (start === 0 || !/[a-z0-9+.-]/.test(lowercaseValue[start - 1]))) {
    cursor += schemeLength;
    const colonLength = troubleshootingTokenLengthAt(lowercaseValue, cursor, TROUBLESHOOTING_URL_COLON_TOKENS);
    if (colonLength > 0) cursor += colonLength;
    else schemeLength = 0;
  } else schemeLength = 0;
  if (schemeLength === 0) cursor = start;
  const firstSlash = troubleshootingTokenLengthAt(lowercaseValue, cursor, TROUBLESHOOTING_URL_SLASH_TOKENS);
  if (firstSlash === 0) return 0;
  cursor += firstSlash;
  const secondSlash = troubleshootingTokenLengthAt(lowercaseValue, cursor, TROUBLESHOOTING_URL_SLASH_TOKENS);
  return secondSlash > 0 ? cursor + secondSlash : 0;
}

function redactTroubleshootingUrlUserinfo(value) {
  const lowercaseValue = value.toLowerCase();
  const output = [];
  let copyStart = 0;
  let index = 0;
  while (index < value.length) {
    const prefixEnd = troubleshootingUrlPrefixEnd(lowercaseValue, index);
    if (prefixEnd === 0) {
      index += 1;
      continue;
    }
    let authorityEnd = prefixEnd;
    let atEnd = 0;
    while (authorityEnd < value.length) {
      const character = value[authorityEnd];
      if (/\s/.test(character) || character === "/" || character === "?" || character === "#") break;
      const atLength = troubleshootingTokenLengthAt(lowercaseValue, authorityEnd, TROUBLESHOOTING_URL_AT_TOKENS);
      if (atLength > 0) {
        atEnd = authorityEnd + atLength;
        authorityEnd = atEnd;
        continue;
      }
      authorityEnd += 1;
    }
    if (atEnd > 0) {
      output.push(value.slice(copyStart, prefixEnd), "[redacted]@");
      copyStart = atEnd;
      index = atEnd;
    } else index = Math.max(index + 1, authorityEnd);
  }
  output.push(value.slice(copyStart));
  return output.join("");
}

function troubleshootingCookieSkipSpace(line, lowercaseLine, start, end) {
  let cursor = start;
  while (cursor < end) {
    if (line[cursor] === " " || line[cursor] === "\t") {
      cursor += 1;
      continue;
    }
    const encodedSpaceLength = troubleshootingTokenLengthAt(
      lowercaseLine,
      cursor,
      TROUBLESHOOTING_COOKIE_SPACE_TOKENS
    );
    if (encodedSpaceLength > 0) {
      cursor += encodedSpaceLength;
      continue;
    }
    break;
  }
  return cursor;
}

function troubleshootingCookieValueStart(line, lowercaseLine, start, end, key) {
  let cursor = troubleshootingCookieSkipSpace(line, lowercaseLine, start, end);
  if (!lowercaseLine.startsWith(key, cursor)) return -1;
  cursor += key.length;
  cursor = troubleshootingCookieSkipSpace(line, lowercaseLine, cursor, end);
  const equalsLength = troubleshootingTokenLengthAt(
    lowercaseLine,
    cursor,
    TROUBLESHOOTING_COOKIE_EQUALS_TOKENS
  );
  if (equalsLength === 0) return -1;
  cursor += equalsLength;
  return troubleshootingCookieSkipSpace(line, lowercaseLine, cursor, end);
}

function troubleshootingCookieFieldRanges(lowercaseLine) {
  const ranges = [];
  let start = 0;
  let cursor = 0;
  while (cursor < lowercaseLine.length) {
    const separatorLength = troubleshootingTokenLengthAt(
      lowercaseLine,
      cursor,
      TROUBLESHOOTING_COOKIE_SEMICOLON_TOKENS
    );
    if (separatorLength === 0) {
      cursor += 1;
      continue;
    }
    ranges.push([start, cursor]);
    cursor += separatorLength;
    start = cursor;
  }
  ranges.push([start, lowercaseLine.length]);
  return ranges;
}

function redactTroubleshootingDsmSessionCookieIds(value) {
  return value.split("\n").map((sourceLine) => {
    const lowercaseLine = sourceLine.toLowerCase();
    const fields = troubleshootingCookieFieldRanges(lowercaseLine);
    const hasStayLogin = fields.some(([start, end]) => {
      const valueStart = troubleshootingCookieValueStart(
        sourceLine,
        lowercaseLine,
        start,
        end,
        "stay_login"
      );
      if (valueStart < 0 || lowercaseLine[valueStart] !== "1") return false;
      return troubleshootingCookieSkipSpace(sourceLine, lowercaseLine, valueStart + 1, end) === end;
    });
    if (!hasStayLogin) return sourceLine;
    const idRanges = fields.map(([start, end]) => [
      troubleshootingCookieValueStart(sourceLine, lowercaseLine, start, end, "id"), end
    ]).filter(([start]) => start >= 0);
    if (!idRanges.length) return sourceLine;
    const output = [];
    let cursor = 0;
    for (const [start, end] of idRanges) {
      output.push(sourceLine.slice(cursor, start), "[redacted]");
      cursor = end;
    }
    output.push(sourceLine.slice(cursor));
    return output.join("");
  }).join("\n");
}

function boundedSanitizedTroubleshootingText(value, limit = TROUBLESHOOTING_RECORD_LIMIT) {
  const maximum = Math.max(128, Math.min(TROUBLESHOOTING_VISIBLE_LIMIT, Number(limit) || TROUBLESHOOTING_RECORD_LIMIT));
  const text = typeof value === "string" ? value : "";
  if (text.length <= maximum) return text.trim();
  const suffix = "\n[truncated: bounded troubleshooting copy]";
  return `${text.slice(0, Math.max(0, maximum - suffix.length)).trimEnd()}${suffix}`;
}

function redactedTroubleshootingText(value) {
  let text = typeof value === "string" ? value : "";
  text = text
    .replace(/\r\n?/g, "\n")
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, " ")
    .replace(/\b(authorization|proxy-authorization|cookie|set-cookie)\s*:[^\n]*/gi, "$1: [redacted]")
    .replace(/\b(bearer|basic)\s+[a-z0-9._~+/=-]+/gi, "$1 [redacted]");
  text = redactTroubleshootingUrlUserinfo(text);
  text = redactTroubleshootingDsmSessionCookieIds(text);
  text = redactTroubleshootingCredentialFields(text);
  return text;
}

function sanitizedTroubleshootingText(value, limit = TROUBLESHOOTING_RECORD_LIMIT) {
  return boundedSanitizedTroubleshootingText(redactedTroubleshootingText(value), limit);
}

function troubleshootingField(value, fallback) {
  return sanitizedTroubleshootingText(String(value || fallback || ""), ACTIVITY_FIELD_LIMIT)
    .replace(/\s+/g, " ")
    .trim() || fallback;
}

function activityTroubleshootingText(value) {
  const event = normalizedActivityEvent(value);
  if (!event) return "";
  const lines = [
    "Synology Drive Sync activity event",
    `Time: ${troubleshootingField(formatDate(event.epoch), "Unavailable")}`,
    `Epoch: ${event.epoch || "Unavailable"}`,
    `Code: ${troubleshootingField(event.code, "unknown.event")}`,
    `Profile: ${troubleshootingField(event.profile, "none")}`,
    `State: ${troubleshootingField(event.state, "unknown")}`,
    `Category: ${troubleshootingField(event.category, "operations")}`,
    `Level: ${troubleshootingField(event.level, "info")}`
  ];
  if (event.client_request_id) lines.push(`Client request ID: ${event.client_request_id}`);
  if (event.doctor_inventory) {
    const evidence = event.doctor_inventory.inventory;
    lines.push(`Doctor discovery scope: ${doctorInventoryScopeLabel(evidence.scope)}`);
    if (evidence.scope === "visible_shared_folders") {
      lines.push("Discovery only: File Station reported these roots; browse and write permission were not tested.");
    }
    lines.push(`Doctor discovery entries: ${evidence.total}; displayed: ${evidence.entries.length}${evidence.truncated ? "; sample truncated" : ""}`);
    for (const entry of evidence.entries.slice(0, 5)) {
      lines.push(`- ${doctorText(entry.kind, "entry", 16)}: ${doctorText(entry.path, "entry", 512)} (name=${doctorText(entry.name, "entry", 256)})`);
    }
  } else if (event.message) lines.push(`Message:\n${event.message}`);
  return boundedSanitizedTroubleshootingText(lines.join("\n"), TROUBLESHOOTING_RECORD_LIMIT);
}

function normalizedDoctorLevel(value) {
  const level = String(value || "").trim().toLowerCase();
  return DOCTOR_LEVELS.includes(level) ? level : "standard";
}

function doctorLevelRank(value) {
  return { quick: 0, standard: 1, extensive: 2 }[normalizedDoctorLevel(value)];
}

function expectedDoctorSections(level, writeTest, state = "pending") {
  const rank = doctorLevelRank(level);
  return DOCTOR_SECTION_CATALOG.filter((section) => {
    if (section.minimum === "write") return writeTest === true;
    return doctorLevelRank(section.minimum) <= rank;
  }).map((section) => ({
    id: section.id,
    label: section.label,
    state,
    detail: state === "skipped"
      ? "The installed package returned no structured evidence for this diagnostic area."
      : section.detail,
    step: null,
    duration_ms: null,
    timing_scope: "",
    checks: [],
    inventory: null,
    profile: ""
  }));
}

function emptyDoctorProgress() {
  return { active: false, phase: "idle", level: "standard", started_epoch: 0 };
}

function doctorSummary(sections) {
  const summary = { ok: 0, warn: 0, failed: 0, skipped: 0, pending: 0, running: 0, total: 0 };
  for (const section of Array.isArray(sections) ? sections : []) {
    const state = Object.prototype.hasOwnProperty.call(summary, section.state) ? section.state : "warn";
    summary[state] += 1;
    summary.total += 1;
  }
  return summary;
}

function doctorState(value, fallback = "warn") {
  const normalized = String(value === true ? "ok" : (value === false ? "failed" : value || ""))
    .trim()
    .toLowerCase()
    .replace(/[\s-]+/g, "_");
  return DOCTOR_STATE_ALIASES[normalized] || (DOCTOR_STATE_ALIASES[fallback] || fallback);
}

function aggregateDoctorState(items, fallback = "warn") {
  const states = (Array.isArray(items) ? items : []).map((item) => doctorState(item && item.state, "warn"));
  for (const state of ["failed", "warn", "running", "pending", "ok", "skipped"]) {
    if (states.includes(state)) return state;
  }
  return doctorState(fallback, "warn");
}

function doctorDuration(value) {
  const number = Number(value);
  return Number.isFinite(number) && number >= 0 ? Math.round(number) : null;
}

// Execution order, which is deliberately not display order: the CLI groups
// sections for reading and reports the step separately. Rendering the grouping
// without the step would assert a sequence that is not the one that ran.
function doctorStep(value) {
  const number = Number(value);
  return Number.isInteger(number) && number >= 1 && number <= 999 ? number : null;
}

function doctorText(value, fallback = "", limit = 4096) {
  return boundedSanitizedTroubleshootingText(
    redactedTroubleshootingText(typeof value === "string" || typeof value === "number" ? String(value) : fallback),
    Math.max(128, Math.min(8192, Number(limit) || 4096))
  ).replace(/\s+/g, " ").trim() || fallback;
}

function doctorIdentifier(value, fallback) {
  const identifier = String(value || "")
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "_")
    .replace(/^_+|_+$/g, "")
    .slice(0, 64);
  return identifier || fallback;
}

function boundedDoctorOutput(value) {
  const source = typeof value === "string" ? value : "";
  const receivedBytes = utf8ByteLength(source);
  if (receivedBytes <= DOCTOR_OUTPUT_LIMIT_BYTES) {
    return { text: source, received_bytes: receivedBytes, contract_truncated: false };
  }
  let bytes = 0;
  let text = "";
  for (const character of source) {
    const point = character.codePointAt(0);
    const width = point <= 0x7f ? 1 : (point <= 0x7ff ? 2 : (point <= 0xffff ? 3 : 4));
    if (bytes + width > DOCTOR_OUTPUT_LIMIT_BYTES) break;
    text += character;
    bytes += width;
  }
  return { text, received_bytes: receivedBytes, contract_truncated: true };
}

function doctorOutputEnvelope(result) {
  const value = result && typeof result === "object" && !Array.isArray(result)
    ? (typeof result.output === "string" && result.output.length ? result.output : result.message)
    : result;
  const bounded = boundedDoctorOutput(value);
  let malformedNdjson = false;
  let sawJsonLine = false;
  let lastJsonLine = -1;
  let packageTruncationMarker = false;
  const lines = bounded.text.split("\n");
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index];
    const trimmed = line.trim();
    if (!trimmed.startsWith("{")) continue;
    try {
      const parsed = JSON.parse(trimmed);
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        sawJsonLine = true;
        lastJsonLine = index;
        if (String(parsed.schema || "").toLowerCase() === "sdsync.dsm-output-truncated.v1"
          && parsed.truncated === true) packageTruncationMarker = true;
      }
    } catch (_error) {
      malformedNdjson = true;
    }
  }
  const explicitTruncation = Boolean(result && typeof result === "object" && !Array.isArray(result)
    && (result.output_truncated === true || result.terminal_output_truncated === true || result.output_complete === false))
    || packageTruncationMarker;
  const legacyBoundary = !bounded.contract_truncated
    && !explicitTruncation
    && bounded.received_bytes === 64 * 1024;
  const truncated = bounded.contract_truncated || explicitTruncation || legacyBoundary;
  const incomplete = truncated || malformedNdjson;
  const reasons = [];
  if (bounded.contract_truncated) reasons.push("output exceeded the one-MiB DSM API response contract");
  if (explicitTruncation) reasons.push("the package marked terminal output as truncated");
  if (legacyBoundary) reasons.push("output ended exactly at the legacy 64-KiB capture boundary");
  if (malformedNdjson) reasons.push("at least one NDJSON record was incomplete or malformed");
  const trailingText = lastJsonLine < 0
    ? ""
    : doctorText(
      lines.slice(lastJsonLine + 1)
        .map((line) => line.trim())
        .filter((line) => line && !line.startsWith("{"))
        .slice(0, 8)
        .join(" · "),
      "",
      4096
    );
  return {
    text: bounded.text,
    received_bytes: bounded.received_bytes,
    truncated,
    incomplete,
    saw_json_line: sawJsonLine,
    trailing_text: trailingText,
    detail: reasons.join("; ")
  };
}

function doctorInventoryEntry(value, index) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const rawKind = String(value.kind || value.type || value.entry_type || "entry").toLowerCase();
  const kind = rawKind.includes("dir") || rawKind === "folder"
    ? "folder"
    : (rawKind.includes("file") || rawKind === "regular" ? "file" : (rawKind === "symlink" ? "link" : "entry"));
  const path = doctorText(
    value.relative_path || value.relative || value.path || value.name,
    `Entry ${index + 1}`,
    512
  );
  const name = doctorText(value.name || value.basename, path.split("/").filter(Boolean).pop() || path, 256);
  const sizeCandidate = value.size_bytes !== undefined ? value.size_bytes : value.size;
  const parsedSize = Number(sizeCandidate);
  const modified = value.modified_epoch !== undefined
    ? value.modified_epoch
    : (value.mtime_epoch !== undefined ? value.mtime_epoch : (value.mtime_seconds !== undefined ? value.mtime_seconds : (value.modified_at || value.mtime || "")));
  return {
    path,
    name,
    kind,
    size_bytes: Number.isFinite(parsedSize) && parsedSize >= 0 ? Math.round(parsedSize) : null,
    modified: typeof modified === "number" || typeof modified === "string" ? modified : "",
    relative_path_truncated: value.relative_path_truncated === true,
    name_truncated: value.name_truncated === true,
    mount_boundary: value.mount_boundary === true
  };
}

function normalizedDoctorInventory(value, fallbackTotal = null) {
  const inventory = Array.isArray(value) ? { entries: value } : value;
  if (!inventory || typeof inventory !== "object") return null;
  const candidates = Array.isArray(inventory.entries)
    ? inventory.entries
    : (Array.isArray(inventory.sample) ? inventory.sample : (Array.isArray(inventory.items) ? inventory.items : []));
  const entries = candidates.map(doctorInventoryEntry).filter(Boolean).slice(0, 5);
  const totalCandidate = inventory.total !== undefined
    ? inventory.total
    : (inventory.total_entries !== undefined ? inventory.total_entries : (inventory.total_count !== undefined ? inventory.total_count : (inventory.count !== undefined ? inventory.count : fallbackTotal)));
  const parsedTotal = Number(totalCandidate);
  const total = Number.isFinite(parsedTotal) && parsedTotal >= 0 ? Math.max(entries.length, Math.round(parsedTotal)) : candidates.length;
  const rawScope = String(inventory.scope || "").trim().toLowerCase();
  const scope = ["direct_children", "visible_shared_folders"].includes(rawScope)
    ? rawScope
    : "direct_children";
  return {
    entries,
    total,
    truncated: inventory.truncated === true || candidates.length > 5 || total > entries.length,
    scope
  };
}

function doctorInventoryScopeLabel(value) {
  return value === "visible_shared_folders" ? "Visible shared folders" : "Direct children";
}

function normalizedDoctorInventoryRecord(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  if (String(value.schema || "").toLowerCase() !== "sdsync.dsm-doctor-inventory.v1") return null;
  if (String(value.action || "").toLowerCase() !== "doctor") return null;
  const nestedInventory = value.inventory && typeof value.inventory === "object" && !Array.isArray(value.inventory)
    ? value.inventory
    : null;
  const inventory = normalizedDoctorInventory(nestedInventory || {
    scope: value.scope,
    total_entries: value.total_entries,
    truncated: value.truncated,
    sample: Array.isArray(value.sample) ? value.sample.slice(0, 5) : []
  });
  if (!inventory) return null;
  return {
    schema: "sdsync.dsm-doctor-inventory.v1",
    epoch: numberOr(value.epoch, 0),
    action: "doctor",
    profile: doctorText(value.profile, "none", 128),
    inventory
  };
}

function doctorInventoryRecordsFromText(value, limit = 20) {
  const records = [];
  const maximum = Math.max(1, Math.min(50, Number(limit) || 20));
  for (const line of String(value || "").split("\n")) {
    const trimmed = line.trim();
    if (!trimmed.startsWith("{") || !trimmed.endsWith("}")) continue;
    try {
      const record = normalizedDoctorInventoryRecord(JSON.parse(trimmed));
      if (record) records.push(record);
    } catch (_error) { /* Non-Doctor log lines remain plain troubleshooting evidence. */ }
  }
  return records.slice(-maximum);
}

// Every package log stream stamps its own records, so a rendered time is read
// back from the record rather than derived from when the browser saw it. The
// shell, controller, scheduler, doctor, API and audit writers all emit whole
// seconds in `epoch`; the core's own `sdsync.log.v1` sync stream emits
// `timestamp_ms`. Both are named machine-readable fields -- nothing here looks
// for a date inside the message text, and nothing invents one. A line that
// carries neither field (a rotation-boundary partial, or a record clipped by
// the bridge's 8192-byte per-line ceiling) reports 0 so the view can say the
// time was not recorded instead of showing a plausible wrong one.
function logLineEpochSeconds(line) {
  const trimmed = String(line || "").trim();
  if (!trimmed.startsWith("{") || !trimmed.endsWith("}")) return 0;
  let record = null;
  try { record = JSON.parse(trimmed); } catch (_error) { return 0; }
  if (!record || typeof record !== "object" || Array.isArray(record)) return 0;
  const seconds = Number(record.epoch);
  if (Number.isFinite(seconds) && seconds > 0) return Math.floor(seconds);
  const milliseconds = Number(record.timestamp_ms);
  if (Number.isFinite(milliseconds) && milliseconds > 0) return Math.floor(milliseconds / 1000);
  return 0;
}

function logLinesWithTime(value) {
  return String(value || "").split("\n").map((line, index) => ({
    id: index,
    epoch: logLineEpochSeconds(line),
    text: line
  }));
}

function doctorInventoryRecordFromActivityMessage(value) {
  const prefix = "Doctor inventory evidence ";
  const message = typeof value === "string" ? value : "";
  if (!message.startsWith(prefix)) return null;
  try { return normalizedDoctorInventoryRecord(JSON.parse(message.slice(prefix.length))); }
  catch (_error) { return null; }
}

function normalizedDoctorCheck(value, index) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const id = doctorIdentifier(value.id || value.code || value.name || value.label, `check_${index + 1}`);
  return {
    id,
    label: doctorText(value.label || value.title || value.name || value.code, `Check ${index + 1}`, 256),
    state: doctorState(value.state || value.status || value.result || value.ok, "warn"),
    detail: doctorText(value.detail || value.message || value.summary || value.reason || value.output, "No detail was reported."),
    duration_ms: doctorDuration(value.duration_ms !== undefined ? value.duration_ms : value.elapsed_ms)
  };
}

function normalizedDoctorSection(value, index, profile = "") {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const id = doctorIdentifier(value.id || value.code || value.name || value.label || value.area, `section_${index + 1}`);
  const checksSource = Array.isArray(value.checks)
    ? value.checks
    : (Array.isArray(value.tests) ? value.tests : (Array.isArray(value.results) ? value.results : []));
  const checks = checksSource.map(normalizedDoctorCheck).filter(Boolean);
  const explicitState = value.state !== undefined
    ? value.state
    : (value.status !== undefined ? value.status : (value.result !== undefined ? value.result : value.ok));
  const inventorySource = value.remote_inventory !== undefined
    ? value.remote_inventory
    : (value.inventory !== undefined ? value.inventory : (id.includes("inventory") ? value.entries : null));
  return {
    id,
    step: doctorStep(value.step),
    label: doctorText(value.label || value.title || value.name || value.area || value.code, `Diagnostic section ${index + 1}`, 256),
    state: explicitState === undefined || explicitState === null || explicitState === ""
      ? aggregateDoctorState(checks, "warn")
      : doctorState(explicitState, "warn"),
    detail: doctorText(value.detail || value.message || value.summary || value.reason || value.output, checks.length ? "See the individual checks below." : "No detail was reported."),
    duration_ms: doctorDuration(value.duration_ms !== undefined ? value.duration_ms : value.elapsed_ms),
    timing_scope: doctorText(value.timing_scope || value.timingScope, "", 64),
    checks,
    inventory: normalizedDoctorInventory(inventorySource, value.remote_total || value.entry_count),
    profile: doctorText(value.profile || profile, "", 128)
  };
}

function doctorDocumentSignal(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const schema = String(value.schema || "").toLowerCase();
  return schema === "sdsync.doctor.v1"
    || ["sections", "stages", "diagnostics", "profiles"].some((key) => Array.isArray(value[key]))
    || (value.remote_inventory && typeof value.remote_inventory === "object")
    || String(value.kind || value.operation || "").toLowerCase() === "doctor";
}

function doctorDocumentFromCandidate(value, depth = 0) {
  if (!value || typeof value !== "object" || Array.isArray(value) || depth > 3) return null;
  const schema = String(value.schema || "").toLowerCase();
  if (schema === "sdsync.doctor-job.v1") return doctorDocumentFromCandidate(value.doctor, depth + 1);
  if (schema.includes("source") && schema.includes("doctor")) return null;
  if (doctorDocumentSignal(value)) return value;
  for (const key of ["doctor", "diagnostic", "report", "result", "data"]) {
    const nested = doctorDocumentFromCandidate(value[key], depth + 1);
    if (nested) return nested;
  }
  return null;
}

function parsedJsonCandidates(value) {
  const text = boundedDoctorOutput(value).text.trim();
  if (!text) return [];
  const candidates = [];
  const serialized = new Set();
  const remember = (candidate) => {
    if (!candidate || typeof candidate !== "object" || Array.isArray(candidate)) return;
    let signature = "";
    try { signature = JSON.stringify(candidate); } catch (_error) { return; }
    if (!signature || serialized.has(signature)) return;
    serialized.add(signature);
    candidates.push(candidate);
  };
  try { remember(JSON.parse(text)); } catch (_error) { /* Terminal output may surround the JSON record. */ }
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed.startsWith("{") || !trimmed.endsWith("}")) continue;
    try { remember(JSON.parse(trimmed)); } catch (_error) { /* Continue to the balanced-object scan. */ }
  }
  let start = -1;
  let depth = 0;
  let quoted = false;
  let escaped = false;
  for (let index = 0; index < text.length && candidates.length < 128; index += 1) {
    const character = text[index];
    if (quoted) {
      if (escaped) escaped = false;
      else if (character === "\\") escaped = true;
      else if (character === "\"") quoted = false;
      continue;
    }
    if (character === "\"") { quoted = true; continue; }
    if (character === "{") {
      if (depth === 0) start = index;
      depth += 1;
    } else if (character === "}" && depth > 0) {
      depth -= 1;
      if (depth === 0 && start >= 0) {
        try { remember(JSON.parse(text.slice(start, index + 1))); } catch (_error) { /* Ignore non-JSON braces. */ }
        start = -1;
      }
    }
  }
  return candidates;
}

function doctorDocumentFromResult(result, outputOverride = undefined) {
  if (result && typeof result === "object" && !Array.isArray(result)) {
    for (const candidate of [result.doctor, result.diagnostic, result.report, result]) {
      const document = doctorDocumentFromCandidate(candidate);
      if (document) return document;
    }
  }
  const output = outputOverride !== undefined
    ? outputOverride
    : (result && typeof result === "object" ? result.output : result);
  const candidates = parsedJsonCandidates(output);
  const batch = candidates.slice().reverse().find((candidate) => String(candidate.schema || "").toLowerCase() === "sdsync.doctor-batch.v1") || null;
  const jobs = candidates.filter((candidate) => String(candidate.schema || "").toLowerCase() === "sdsync.doctor-job.v1")
    .map((candidate) => {
      const doctor = doctorDocumentFromCandidate(candidate.doctor);
      const profile = doctorText(candidate.profile || candidate.name || (doctor && doctor.profile), "Unknown profile", 128);
      if (doctor) return Object.assign({}, doctor, { profile });
      const state = doctorState(candidate.status, candidate.error ? "failed" : "skipped");
      return {
        schema: "sdsync.doctor.v1",
        level: (batch && batch.level) || "standard",
        status: state,
        profile,
        sections: [{
          id: "doctor_job",
          label: "Profile Doctor execution",
          status: state,
          detail: doctorText(candidate.error, state === "skipped" ? "The profile was not run." : "The profile Doctor did not return a structured report.")
        }]
      };
    }).filter(Boolean);
  if (jobs.length) return Object.assign({}, batch || {}, { schema: "sdsync.doctor-batch.v1", level: (batch && batch.level) || jobs[0].level, profiles: jobs });
  if (batch) return batch;
  for (let index = candidates.length - 1; index >= 0; index -= 1) {
    const document = doctorDocumentFromCandidate(candidates[index]);
    if (document) return document;
  }
  return null;
}

function sectionsFromDoctorDocument(document) {
  if (!document || typeof document !== "object") return [];
  const sections = [];
  const append = (values, profile = "") => {
    if (!Array.isArray(values)) return;
    for (const value of values) {
      const section = normalizedDoctorSection(value, sections.length, profile);
      if (section) sections.push(section);
    }
  };
  append(document.sections || document.stages || document.diagnostics || document.areas);
  if (Array.isArray(document.profiles)) {
    for (const profile of document.profiles) {
      if (!profile || typeof profile !== "object" || Array.isArray(profile)) continue;
      const evidence = profile.doctor && typeof profile.doctor === "object" && !Array.isArray(profile.doctor) ? profile.doctor : profile;
      const profileName = profile.profile || profile.name || evidence.profile;
      const firstSection = sections.length;
      append(evidence.sections || evidence.stages || evidence.diagnostics || evidence.areas, profileName);
      const profileInventory = normalizedDoctorInventory(
        evidence.remote_inventory || evidence.inventory,
        evidence.remote_total || evidence.entry_count
      );
      if (profileInventory) {
        const inventorySection = sections.slice(firstSection).find((section) => section.id.includes("inventory"));
        if (inventorySection) inventorySection.inventory = profileInventory;
      }
    }
  }
  if (!sections.length && Array.isArray(document.checks)) append(document.checks);
  const topInventory = normalizedDoctorInventory(
    document.remote_inventory || document.inventory,
    document.remote_total || document.entry_count
  );
  if (topInventory) {
    const inventorySection = sections.find((section) => section.id.includes("inventory"));
    if (inventorySection) inventorySection.inventory = topInventory;
    else sections.push({
      id: "inventory",
      label: "Remote inventory sample",
      state: doctorState((document.remote_inventory || {}).state || document.status, "ok"),
      detail: "Bounded remote discovery evidence returned by the target.",
      step: null,
      duration_ms: null,
      timing_scope: "",
      checks: [],
      inventory: topInventory,
      profile: ""
    });
  }
  return sections;
}

function doctorOperationFailureDetail(result, rawOutput) {
  const evidence = [];
  const remember = (value) => {
    const detail = doctorText(value, "", 2048);
    if (detail && !evidence.includes(detail)) evidence.push(detail);
  };
  if (result && typeof result === "object" && !Array.isArray(result)) {
    for (const key of ["message", "error", "detail", "reason"]) remember(result[key]);
  }
  for (const candidate of parsedJsonCandidates(rawOutput)) {
    const schema = String(candidate.schema || "").toLowerCase();
    const failed = candidate.ok === false || doctorState(candidate.status || candidate.state, "skipped") === "failed";
    if (!failed && !(schema.includes("source") && schema.includes("doctor"))) continue;
    for (const key of ["message", "error", "detail", "reason", "code", "stage"]) remember(candidate[key]);
    const sections = Array.isArray(candidate.sections) ? candidate.sections : [];
    for (const section of sections) {
      if (doctorState(section && (section.status || section.state), "skipped") !== "failed") continue;
      remember(section.detail || section.message || section.reason || section.error);
    }
  }
  for (const line of String(rawOutput || "").split("\n").slice(-256)) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("{")) continue;
    remember(trimmed);
  }
  return doctorText(
    evidence.slice(0, 8).join(" · "),
    "The package or DSM API reported that the Target Doctor operation did not complete successfully.",
    4096
  );
}

function doctorSourceFailureDetail(rawOutput) {
  const evidence = [];
  let sourceFailed = false;
  const remember = (value) => {
    const detail = doctorText(value, "", 2048);
    if (detail && !evidence.includes(detail)) evidence.push(detail);
  };
  for (const candidate of parsedJsonCandidates(rawOutput)) {
    const schema = String(candidate.schema || "").toLowerCase();
    const sourceCandidate = schema === "sdsync.dsm-source-check.v1"
      || (schema.includes("source") && schema.includes("doctor"));
    if (!sourceCandidate) continue;
    const status = String(candidate.status || candidate.state || "").trim().toLowerCase();
    const failedSections = (Array.isArray(candidate.sections) ? candidate.sections : [])
      .filter((section) => doctorState(section && (section.status || section.state), "skipped") === "failed");
    const failed = schema === "sdsync.dsm-source-check.v1"
      || candidate.ok === false
      || ["error", "fail", "failed", "failure", "partial"].includes(status)
      || Boolean(candidate.error)
      || failedSections.length > 0;
    if (!failed) continue;
    sourceFailed = true;
    for (const key of ["message", "error", "detail", "reason", "code", "stage"]) remember(candidate[key]);
    for (const section of failedSections) remember(section.detail || section.message || section.reason || section.error);
  }
  if (!sourceFailed) return "";
  return doctorText(
    evidence.slice(0, 8).join(" · "),
    "The package-local source diagnostic failed.",
    4096
  );
}

function doctorReportFromResult(result, successful, requestedLevel, writeTest, startedEpoch = 0) {
  const level = normalizedDoctorLevel(requestedLevel);
  const outputEnvelope = doctorOutputEnvelope(result);
  const rawOutput = outputEnvelope.text;
  const document = doctorDocumentFromResult(result, rawOutput);
  let sections = sectionsFromDoctorDocument(document);
  const hasProfileGroups = Boolean(document && Array.isArray(document.profiles) && document.profiles.length);
  if (document && !hasProfileGroups) {
    const returnedIds = new Set(sections.map((section) => section.id));
    for (const expected of expectedDoctorSections(level, writeTest, "skipped")) {
      if (!returnedIds.has(expected.id)) sections.push(expected);
    }
  } else if (!document) {
    sections = expectedDoctorSections(level, writeTest, "skipped");
    sections.push({
      id: "terminal_evidence",
      label: "Legacy terminal evidence",
      state: successful ? "ok" : "failed",
      detail: doctorText(rawOutput, successful ? "Doctor completed." : "Doctor failed."),
      step: null,
      duration_ms: null,
      timing_scope: "operation",
      checks: [],
      inventory: null,
      profile: ""
    });
  }
  const sourceFailureDetail = doctorSourceFailureDetail(rawOutput);
  if (sourceFailureDetail) {
    sections.push({
      id: "package_source_diagnostic",
      label: "Package-local source diagnostic",
      state: "failed",
      detail: sourceFailureDetail,
      step: null,
      duration_ms: null,
      timing_scope: "operation",
      checks: [],
      inventory: null,
      profile: ""
    });
  }
  if (!successful && document && !sections.some((section) => doctorState(section.state, "warn") === "failed")) {
    sections.push({
      id: "package_operation",
      label: "Package Doctor operation",
      state: "failed",
      detail: doctorOperationFailureDetail(result, rawOutput),
      step: null,
      duration_ms: null,
      timing_scope: "operation",
      checks: [],
      inventory: null,
      profile: ""
    });
  }
  if (outputEnvelope.incomplete) {
    sections.push({
      id: "terminal_output_integrity",
      label: "Terminal output integrity",
      state: "failed",
      detail: doctorText(
        `The returned Doctor evidence is incomplete and must not be treated as a complete result${outputEnvelope.detail ? `: ${outputEnvelope.detail}` : "."}`,
        "The returned Doctor evidence is incomplete.",
        4096
      ),
      step: null,
      duration_ms: null,
      timing_scope: "transport",
      checks: [],
      inventory: null,
      profile: ""
    });
  }
  if (document && outputEnvelope.trailing_text) {
    sections.push({
      id: "terminal_warning",
      label: "Terminal warning evidence",
      state: "warn",
      detail: outputEnvelope.trailing_text,
      step: null,
      duration_ms: null,
      timing_scope: "transport",
      checks: [],
      inventory: null,
      profile: ""
    });
  }
  const summary = doctorSummary(sections);
  const started = Number(document && (document.started_epoch || document.started_at_epoch)) || Number(startedEpoch) || 0;
  const finished = Number(document && (document.finished_epoch || document.finished_at_epoch)) || 0;
  const reportedDuration = doctorDuration(document && (document.duration_ms !== undefined ? document.duration_ms : document.elapsed_ms));
  const inferredDuration = started > 0 && finished >= started ? (finished - started) * 1000 : null;
  return {
    schema: doctorText(document && document.schema, document ? "sdsync.doctor.v1" : "legacy-output", 128),
    structured: Boolean(document),
    level: normalizedDoctorLevel(document && document.level ? document.level : level),
    state: !successful || summary.failed
      ? "failed"
      : (summary.warn
        ? "warn"
        : (document && (document.status !== undefined || document.state !== undefined)
          ? doctorState(document.status !== undefined ? document.status : document.state, "ok")
          : "ok")),
    started_epoch: started,
    finished_epoch: finished,
    duration_ms: reportedDuration === null ? inferredDuration : reportedDuration,
    summary,
    sections,
    raw_output: rawOutput,
    output_incomplete: outputEnvelope.incomplete,
    output_truncated: outputEnvelope.truncated,
    output_received_bytes: outputEnvelope.received_bytes,
    terminal_warning: outputEnvelope.trailing_text
  };
}

function runningDoctorReport(level, writeTest, startedEpoch) {
  const sections = expectedDoctorSections(level, writeTest, "pending");
  return {
    schema: "sdsync.doctor.v1",
    structured: true,
    level: normalizedDoctorLevel(level),
    state: "running",
    started_epoch: startedEpoch,
    finished_epoch: 0,
    duration_ms: null,
    summary: doctorSummary(sections),
    sections,
    raw_output: "",
    output_incomplete: false,
    output_truncated: false,
    output_received_bytes: 0,
    terminal_warning: ""
  };
}

function idleDoctorReport() {
  return {
    schema: "sdsync.doctor.v1",
    structured: true,
    level: "standard",
    state: "skipped",
    started_epoch: 0,
    finished_epoch: 0,
    duration_ms: null,
    summary: doctorSummary([]),
    sections: [],
    raw_output: "",
    output_incomplete: false,
    output_truncated: false,
    output_received_bytes: 0,
    terminal_warning: ""
  };
}

function doctorDisplayOutput(report, fallback) {
  if (!report || report.structured !== true) return boundedText(fallback, "No diagnostic output was returned.");
  const summary = report.summary || doctorSummary(report.sections);
  return [
    `${String(report.level || "standard").toUpperCase()} Target Doctor returned structured evidence for ${summary.total || 0} section${summary.total === 1 ? "" : "s"}.`,
    `${summary.ok || 0} OK · ${summary.warn || 0} warning · ${summary.failed || 0} not OK · ${summary.skipped || 0} skipped.`,
    report.output_incomplete ? "Terminal output was incomplete; displayed sections are partial and must not be treated as a complete Doctor result." : "Terminal output integrity was preserved within the DSM API response contract.",
    "Raw NDJSON is not rendered; use the bounded section breakdown or Copy diagnostics."
  ].join("\n");
}

function doctorTroubleshootingText(report, title, output) {
  const model = report && typeof report === "object" ? report : runningDoctorReport("standard", false, 0);
  const summary = model.summary || doctorSummary(model.sections);
  const lines = [
    "Synology Drive Sync Target Doctor",
    `Title: ${doctorText(title, "Diagnostic", 256)}`,
    `Schema: ${doctorText(model.schema, "Unavailable", 128)}`,
    `Level: ${normalizedDoctorLevel(model.level)}`,
    `Status: ${doctorState(model.state, "warn")}`,
    `Summary: ${summary.ok || 0} OK, ${summary.warn || 0} warning, ${summary.failed || 0} not OK, ${summary.skipped || 0} skipped`,
    model.duration_ms === null || model.duration_ms === undefined ? "Duration: unavailable" : `Duration: ${model.duration_ms} ms`,
    `Terminal output complete: ${model.output_incomplete === true ? "no" : "yes"}`,
    `Terminal output truncated: ${model.output_truncated === true ? "yes" : "no"}`
  ];
  for (const section of Array.isArray(model.sections) ? model.sections : []) {
    lines.push("", `[${doctorState(section.state, "warn").toUpperCase()}] ${doctorText(section.label, section.id, 256)}`);
    if (section.step) lines.push(`Step: ${section.step}`);
    if (section.profile) lines.push(`Profile: ${doctorText(section.profile, "", 128)}`);
    if (section.duration_ms !== null && section.duration_ms !== undefined) lines.push(`Duration: ${section.duration_ms} ms`);
    if (section.timing_scope) lines.push(`Timing scope: ${doctorText(section.timing_scope, "unavailable", 64)}`);
    lines.push(doctorText(section.detail, "No detail was reported."));
    for (const check of Array.isArray(section.checks) ? section.checks : []) {
      lines.push(`- ${doctorState(check.state, "warn").toUpperCase()}: ${doctorText(check.label, check.id, 256)} — ${doctorText(check.detail, "No detail was reported.")}`);
    }
    if (section.inventory) {
      lines.push(`Discovery scope: ${doctorInventoryScopeLabel(section.inventory.scope)}`);
      if (section.inventory.scope === "visible_shared_folders") {
        lines.push("Discovery only: File Station reported these roots; browse and write permission were not tested.");
      }
      lines.push(`Remote entries: ${section.inventory.total}; displayed: ${section.inventory.entries.length}${section.inventory.truncated ? "; sample truncated" : ""}`);
      for (const entry of section.inventory.entries.slice(0, 5)) {
        const metadata = [
          `name=${doctorText(entry.name, "entry", 256)}`,
          `kind=${entry.kind}`,
          `relative_path_truncated=${entry.relative_path_truncated === true}`,
          `name_truncated=${entry.name_truncated === true}`,
          entry.size_bytes === null ? "" : `size_bytes=${entry.size_bytes}`,
          entry.modified ? `modified=${entry.modified}` : "",
          `mount_boundary=${entry.mount_boundary === true}`
        ].filter(Boolean).join("; ");
        lines.push(`- relative_path=${doctorText(entry.path || entry.name, "entry", 512)}; ${metadata}`);
      }
    }
  }
  // The display summary is intentionally capped at 64 KiB, but credential
  // terminators can occur after that boundary. Prefer the report's complete
  // one-MiB raw contract so redaction sees the whole value before the final
  // troubleshooting-copy bound is applied.
  const terminalOutput = model.structured === true
    ? ""
    : (typeof model.raw_output === "string" && model.raw_output.length
      ? model.raw_output
      : (typeof output === "string" ? output : ""));
  if (terminalOutput) lines.push("", "Raw terminal output", terminalOutput);
  return sanitizedTroubleshootingText(lines.join("\n"), TROUBLESHOOTING_RECORD_LIMIT);
}

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

function partialMutationInspectionRequired(caught, fallback, appliedDetail) {
  const observed = boundedText(caught && caught.message, fallback).slice(0, MUTATION_MESSAGE_LIMIT / 2);
  const failure = new Error(
    `${observed} ${appliedDetail} Do not retry this multi-stage operation; inspect Activity and Logs before taking any further action.`
  );
  failure.name = "PartialMutationInspectionRequiredError";
  failure.outcomeUnknown = Boolean(caught && caught.outcomeUnknown === true);
  failure.requiresInspection = true;
  failure.partialApplication = true;

  const requestId = caught && caught.trustedRequestId === true
    ? validatedClientRequestId(caught.requestId)
    : "";
  const jobId = caught && caught.trustedJobId === true
    ? validatedJobId(caught.jobId)
    : "";
  if (requestId) {
    failure.requestId = requestId;
    failure.trustedRequestId = true;
  }
  if (jobId) {
    failure.jobId = jobId;
    failure.trustedJobId = true;
  }

  for (const field of ["accepted", "acceptanceUnknown", "preAcceptance", "trustedRejection", "csrfRejected", "clientTimeout"]) {
    if (caught && caught[field] === true) failure[field] = true;
  }
  for (const field of ["status", "transportStatus"]) {
    const value = Number(caught && caught[field]);
    if (Number.isInteger(value)) failure[field] = value;
  }
  for (const field of ["code", "stage", "operation"]) {
    const value = boundedText(caught && caught[field], "").slice(0, 128);
    if (value) failure[field] = value;
  }
  return failure;
}

// The Activity route shows two independent read-only feeds. Name whichever one
// is unavailable instead of reporting the pair as dead, so a stalled package
// log scan is not mistaken for a stalled package.
function logsFeedState(logsReady, activityReady, lines) {
  if (logsReady && activityReady) return `Live · ${lines} line limit`;
  if (logsReady) return `Package log live · ${lines} line limit · activity feed unavailable`;
  if (activityReady) return `Activity live · ${lines} line limit · package log read unavailable`;
  return "Logs unavailable";
}

function normalizedActivityEvent(event) {
  if (!event || typeof event !== "object" || Array.isArray(event)) return null;
  // Activity values arrive inside a response that is already globally bounded,
  // but credential terminators may sit beyond the much smaller display limits.
  // Redact the complete API field first so slicing cannot preserve a partial
  // URL userinfo or contextual cookie secret after discarding its terminator.
  const field = (value, fallback, limit = ACTIVITY_FIELD_LIMIT) => redactedTroubleshootingText(
    typeof value === "string" ? value : fallback
  ).slice(0, limit);
  const rawMessage = typeof event.message === "string" ? event.message : "";
  // Parse only the exact private Doctor prefix and strict nested schema before
  // applying generic credential-field redaction. A legitimate logical name
  // such as `/password:visible` otherwise looks like an unquoted credential
  // field and can make the embedded JSON unparsable. The normalizer below
  // still whitelists, bounds, and sanitizes every retained inventory field.
  const doctorInventory = normalizedDoctorInventoryRecord(event.doctor_inventory)
    || doctorInventoryRecordFromActivityMessage(rawMessage);
  const completeMessage = redactedTroubleshootingText(rawMessage).slice(0, 4096);
  const message = completeMessage.slice(0, ACTIVITY_MESSAGE_LIMIT);
  return {
    epoch: numberOr(event.epoch, 0),
    code: field(event.code, "unknown.event"),
    profile: field(event.profile, "none"),
    state: field(event.state, "unknown"),
    category: field(event.category, "operations"),
    level: field(event.level, "info"),
    message,
    client_request_id: validatedClientRequestId(event.client_request_id),
    doctor_inventory: doctorInventory
  };
}

function canonicalProfileConfiguration(value, snapshot = false) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const field = (direct, ...aliases) => snapshot ? pick(value, direct, ...aliases) : value[direct];
  const nullableText = (candidate) => {
    const text = typeof candidate === "string" ? candidate : "";
    return text || null;
  };
  return {
    name: field("name"),
    source: field("source"),
    url: field("url"),
    username: field("username"),
    remote: field("remote", "remote_path"),
    compare: field("compare"),
    jobs: Number(field("jobs")),
    allow_http: field("allow_http") === true,
    delete: field("delete") === true,
    max_delete: Number(field("max_delete")),
    make_default: snapshot ? field("default", "is_default") === true : field("make_default") === true,
    excludes: arrayOf(field("excludes")).map(String),
    allow_empty_source: field("allow_empty_source") === true,
    retries: Number(field("retries")),
    timeout_seconds: Number(field("timeout_seconds", "upload_timeout_seconds", "timeout")),
    connect_timeout_seconds: Number(field("connect_timeout_seconds", "connect_timeout")),
    max_rate_bytes_per_second: Number(field("max_rate_bytes_per_second", "max_rate")) > 0
      ? Number(field("max_rate_bytes_per_second", "max_rate"))
      : null,
    ca_certificate: nullableText(field("ca_certificate")),
    danger_accept_invalid_certs: field("danger_accept_invalid_certs", "danger_invalid_certs") === true,
    verbosity: Number(field("verbosity")),
    quiet: field("quiet") === true,
    log_level: field("log_level"),
    log_format: field("log_format"),
    progress: field("progress"),
    output: field("output"),
    remote_log_url: nullableText(field("remote_log_url")),
    remote_log_mode: field("remote_log_mode")
  };
}

function profileSnapshotMatchesExpected(profile, expected) {
  const observed = canonicalProfileConfiguration(profile, true);
  const submitted = canonicalProfileConfiguration(expected, false);
  return Boolean(observed && submitted && JSON.stringify(observed) === JSON.stringify(submitted));
}

function options(entries) {
  return entries.map(([value, label]) => ({ value, label }));
}

// ---------------------------------------------------------------------------
// Per-file sync status
//
// The query engine holds no index: every page is rebuilt from both sides on
// request, so a row can never be stale and there is nothing here to cache.
// That also makes a status query expensive, which is why this view never runs
// on a timer and only ever loads when a person asks it to.
// ---------------------------------------------------------------------------
// One table, read two ways: as the state picker's options and as the label for
// a row's own state. A second copy could drift from this one.
const SYNC_STATE_OPTIONS = Object.freeze([
  ["attention", "Needs attention"],
  ["differs", "Differs"],
  ["missing-remote", "Missing remotely"],
  ["remote-only", "Remote only"],
  ["type-conflict", "Type conflict"],
  ["in-sync", "In sync"],
  ["excluded", "Excluded"],
  ["all", "Every state"]
]);
const SYNC_STATE_LABELS = Object.freeze(Object.fromEntries(SYNC_STATE_OPTIONS));
// The engine refuses anything above 200 itself. These are the sizes this window
// offers, so the largest page a person can ask for is the largest that exists.
const SYNC_PAGE_SIZES = Object.freeze([25, 50, 100, 200]);
const SYNC_PAGE_SIZE_DEFAULT = 100;
// [class suffix, label, engine stats field]
const SYNC_STAT_CARDS = Object.freeze([
  ["in-sync", "In sync", "in_sync_files"],
  ["differs", "Differs", "differing_files"],
  ["missing-remote", "Missing remotely", "missing_remote_files"],
  ["remote-only", "Remote only", "remote_only_entries"],
  ["type-conflict", "Type conflicts", "type_conflicts"],
  ["excluded", "Excluded", "excluded_entries"]
]);
// A resync plan may name far more files than a window should draw. The
// overwrite count and byte total below the list stay exact; only the rendered
// path list is bounded.
const RESYNC_PATH_LIMIT = 200;
const UNREADABLE_RESPONSE = "The package returned a response this window cannot read.";
const SYNC_STATUS_IDLE_MESSAGE = "Choose a profile and check its status. Nothing is read from the NAS until you do.";

const ROLLUP_IDLE_MESSAGE = "No profile has recorded totals yet. Check one scope below once, and this summary answers instantly from then on.";
const ROLLUP_SCHEMA = "sdsync.status-rollup-aggregate.v1";

function emptySyncStatusPage() {
  return {
    loaded: false, profile: "", scope: "", compare: "", limit: 0,
    truncated: false, nextCursor: "", entries: [], stats: null
  };
}

function emptyStatusRollup() {
  return {
    loaded: false, complete: false, observedAtEpoch: 0, overlapping: false,
    profilesTotal: 0, profilesObserved: 0, neverObserved: [],
    total: null, totalUnavailableReason: "", profiles: []
  };
}

// Mirrors the core's own `describe_evidence_age` thresholds exactly, so the
// dashboard and `status-rollup` on the command line never describe the same
// evidence as two different ages. A time the browser cannot make sense of --
// unset, or ahead of this clock -- is rendered as an absolute instant rather
// than as an age, because "in 3 minutes" is worse than a date.
function describeEvidenceAge(epoch) {
  const observed = Number(epoch);
  if (!Number.isFinite(observed) || observed <= 0) return "never observed";
  const seconds = Math.floor(Date.now() / 1000) - observed;
  if (seconds < 0) return formatDate(observed);
  if (seconds < 90) return "moments ago";
  if (seconds < 3 * 3600) return `${Math.floor(seconds / 60)} minutes ago`;
  if (seconds < 48 * 3600) return `${Math.floor(seconds / 3600)} hours ago`;
  return `${Math.floor(seconds / 86400)} days ago`;
}

function rollupCount(value) {
  const number = Number(value);
  return Number.isFinite(number) && number > 0 ? Math.floor(number) : 0;
}

function rollupFilesAndBytes(value) {
  const source = value && typeof value === "object" ? value : {};
  return { files: rollupCount(source.files), bytes: rollupCount(source.bytes) };
}

function normalizedRollupState(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  return {
    inSync: rollupFilesAndBytes(value.in_sync),
    wouldTransfer: rollupFilesAndBytes(value.would_transfer),
    attention: rollupCount(value.attention_entries),
    totalEntries: rollupCount(value.total_entries)
  };
}

// The core establishes every figure here; this only shapes them for rendering.
// Nothing is summed, inferred, or defaulted into existence: a `total` the core
// withheld stays withheld, and its stated reason is carried through so the view
// can say why rather than leaving a reader to add the rows up by hand and reach
// the same wrong answer the core refused to state.
function normalizedStatusRollup(model) {
  if (!model || typeof model !== "object" || model.schema !== ROLLUP_SCHEMA) return emptyStatusRollup();
  const profiles = arrayOf(model.profiles).slice(0, 64).map((entry) => {
    const source = entry && typeof entry === "object" ? entry : {};
    const observation = source.observation && typeof source.observation === "object" ? source.observation : {};
    return {
      profile: boundedText(source.profile, "Unnamed").slice(0, 128),
      observedAtEpoch: rollupCount(source.observed_at_epoch),
      complete: observation.complete === true,
      state: normalizedRollupState(source.state)
    };
  }).filter((entry) => entry.state);
  return {
    loaded: true,
    complete: model.complete === true,
    observedAtEpoch: rollupCount(model.observed_at_epoch),
    overlapping: model.overlapping_profiles === true,
    profilesTotal: rollupCount(model.profiles_total),
    profilesObserved: rollupCount(model.profiles_observed),
    neverObserved: arrayOf(model.profiles_never_observed)
      .slice(0, 64)
      .map((name) => boundedText(name, "").slice(0, 128))
      .filter(Boolean),
    total: normalizedRollupState(model.total),
    totalUnavailableReason: boundedText(model.total_unavailable_reason, "").slice(0, 512),
    profiles
  };
}

function emptyResyncPlan() {
  return {
    profile: "", scope: "", ticket: "", staleTicket: "", overwrites: 0,
    overwriteBytes: 0, paths: [], truncatedPaths: false, confirmed: false
  };
}

// Both endpoints carry the engine's own JSON document inside the `output`
// string of the bridge envelope, exactly as the Doctor terminal path does.
function bridgeDocument(result, schema) {
  let parsed = null;
  try {
    parsed = JSON.parse(boundedText(result && result.output, ""));
  } catch (_error) {
    return null;
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return null;
  return parsed.schema === schema ? parsed : null;
}

function syncStatusEntry(record, index) {
  const entry = record && typeof record === "object" ? record : {};
  const local = entry.local && typeof entry.local === "object" ? entry.local : null;
  const remote = entry.remote && typeof entry.remote === "object" ? entry.remote : null;
  const state = boundedText(entry.state, "unknown");
  const relative = boundedText(entry.relative, "");
  let detail = boundedText(entry.detail, "");
  if (!detail && state === "type-conflict") {
    detail = `local ${boundedText(entry.local_kind, "entry")} against remote ${boundedText(entry.remote_kind, "entry")}`;
  }
  if (!detail && entry.exclusion) detail = `excluded by ${boundedText(entry.exclusion, "rule")}`;
  return {
    key: `${index}:${relative}`,
    relative,
    remotePath: boundedText(entry.remote_path, ""),
    kind: boundedText(entry.entry_kind, "entry"),
    state,
    label: SYNC_STATE_LABELS[state] || state,
    detail,
    localSize: local ? numberOr(local.size, 0) : null,
    localEpoch: local ? numberOr(local.mtime_seconds, 0) : null,
    remoteSize: remote ? numberOr(remote.size, 0) : null,
    remoteEpoch: remote ? numberOr(remote.mtime_seconds, 0) : null
  };
}

function syncStatusPage(document, profile) {
  const stats = document.stats && typeof document.stats === "object" ? document.stats : {};
  return {
    loaded: true,
    profile,
    scope: boundedText(document.scope, ""),
    compare: boundedText(document.compare, "unknown"),
    limit: numberOr(document.limit, 0),
    truncated: document.truncated === true,
    nextCursor: boundedText(document.next_cursor, ""),
    entries: arrayOf(document.entries).map(syncStatusEntry),
    // Kept under the engine's own field names rather than renamed: these
    // totals always describe the whole scope, never the returned page, so this
    // window can honestly say "1,204 differ" while drawing 200 rows.
    stats,
    complete: stats.complete === true
  };
}

function resyncPlanFromDocument(document, profile) {
  const paths = arrayOf(document.paths);
  return {
    profile,
    scope: boundedText(document.scope, ""),
    ticket: boundedText(document.ticket, ""),
    staleTicket: boundedText(document.stale_ticket, ""),
    overwrites: numberOr(document.overwrites, 0),
    overwriteBytes: numberOr(document.overwrite_bytes, 0),
    truncatedPaths: paths.length > RESYNC_PATH_LIMIT,
    paths: paths.slice(0, RESYNC_PATH_LIMIT).map((record, index) => ({
      key: `${index}:${boundedText(record && record.relative, "")}`,
      relative: boundedText(record && record.relative, ""),
      bytes: numberOr(record && record.bytes, 0)
    })),
    confirmed: document.confirmed === true
  };
}

export default {
  name: "SynologyDriveSyncApp",
  components: { ActionIcon, ControlHelp, SecurityPanel },
  data() {
    const settings = loadSettings();
    return {
      routes: [
        { id: "overview", title: "Overview", icon: "overview" }, { id: "profiles", title: "Profiles", icon: "profiles" },
        { id: "routines", title: "Routines", icon: "routines" },
        { id: "sync", title: "Sync status", icon: "sync" },
        { id: "health", title: "Health / Doctor", icon: "health" },
        { id: "activity", title: "Activity / Logs", icon: "activity" }, { id: "notifications", title: "Notifications", icon: "notifications" },
        { id: "security", title: "Security", icon: "security" },
        { id: "settings", title: "Settings", icon: "settings" },
        { id: "about", title: "About", icon: "about" }
      ],
      route: "overview", auth: { signal: undefined }, csrfToken: "", snapshot: null,
      connected: false, connectionLabel: "Connecting to package…", freshness: "Waiting for status",
      bridgeIssue: { title: "", message: "" },
      snapshotTimer: 0, logTimer: 0, snapshotLoading: false, snapshotPromise: null, snapshotRefreshQueued: false, snapshotGeneration: 0, logsLoading: false, operationBusy: false,
      autosaveCoordinator: null, autosavePhase: "saved", autosaveMessage: "Autosave ready", alertDirty: false,
      autosaveFailureScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
      autosaveOutcomeUnknownScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
      autosaveInspectionScopes: { profile: false, routine: false, alerts: false, security: false, interface: false },
      autosaveIncidents: Object.fromEntries(AUTOSAVE_SCOPES.map((scope) => [scope, emptyScopeIncident()])),
      profileFailureRecords: emptyProfileFailureRecords(),
      isolatedIncidents: { connection: emptyIsolatedIncident(), operations: emptyIsolatedIncident() },
      settings, profileFilter: "", profileFilterStatus: "all", profileEditorOpen: false, selectedProfile: "", profileForm: emptyProfile(),
      secretModes: { password: "keep", totp: "keep", remote_log_token: "keep" },
      secretValues: { password: "", totp: "", remote_log_token: "" },
      profileConnectionState: "idle", profileConnectionMessage: "Test authentication to unlock the File Station browser.", connectionProof: "", connectionProofExpires: 0, connectionProofTimer: 0, profileConnectionRequest: 0, profileConnectionAutosaveHeld: false,
      profileSaveState: "idle", profileSaveMessage: "", profileCreationProgress: emptyProfileCreationProgress(), profileReconciliationState: "idle",
      incidentProbe: emptyIncidentProbe(), incidentProbeTimer: 0, incidentProbeStep: 0,
      pathBrowser: emptyPathBrowser(),
      routineEditorOpen: false, routineForm: emptyRoutine(), doctorForm: { scope: "all", level: "standard", write_test: false, write_confirm: false },
      syncStatusForm: { profile: "", scope: "", filter: "", state: "attention", limit: SYNC_PAGE_SIZE_DEFAULT, include_excluded: false },
      statusRollup: emptyStatusRollup(), statusRollupLoading: false, statusRollupMessage: ROLLUP_IDLE_MESSAGE,
      syncStatusResult: emptySyncStatusPage(), syncStatusPhase: "idle", syncStatusMessage: SYNC_STATUS_IDLE_MESSAGE,
      syncStatusBusy: false, syncStatusCursors: [], syncStatusPageNumber: 0,
      resyncForm: { scope: "" }, resyncPlan: emptyResyncPlan(), resyncPhase: "idle", resyncMessage: "", resyncBusy: false,
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
      diagnostic: { title: "Not run in this session", output: "No diagnostic output yet." }, doctorReport: idleDoctorReport(), doctorProgress: emptyDoctorProgress(),
      liveProgress: emptyLiveProgress(),
      logsPaused: false, logSource: "all", logLines: 200, logState: "Waiting for logs", logOutput: "No log data yet.", logRecords: [], activityEvents: [], activitySearch: "", activityCategory: "all", activityLevel: "all",
      lastFailureKey: "", toasts: [], toastSequence: 0,
      confirmation: { visible: false, title: "", message: "", button: "Confirm", resolve: null },
      confirmationPriorFocus: null, confirmationKeyHandler: null,
      pathBrowserPriorFocus: null, pathBrowserKeyHandler: null,
      systemLight: false, visibilityHandler: null, beforeUnloadHandler: null, mediaQuery: null, mediaHandler: null,
      toastTimers: [], abortController: null, connectionWatchCleanups: [], disposed: false
    };
  },
  computed: {
    pageTitle() { const found = this.routes.find((item) => item.id === this.route); return found ? found.title : "Overview"; },
    profiles() { return arrayOf(this.snapshot && this.snapshot.profiles); },
    routines() { return arrayOf(this.snapshot && this.snapshot.routines); },
    filteredProfiles() {
      const query = this.profileFilter.trim().toLowerCase();
      const status = this.profileFilterStatus;
      return this.profiles.filter((profile) => {
        const routine = this.routines.find((item) => String(item.profile) === String(profile.name));
        const searchable = [profile.name, profile.source, profile.url, profile.username, profile.remote, profile.remote_path]
          .map((value) => String(value || "").toLowerCase()).join("\n");
        const statusMatches = status === "all"
          || (status === "ready" && profile.has_password === true)
          || (status === "needs-password" && profile.has_password !== true)
          || (status === "default" && (profile.is_default === true || profile.default === true))
          || (status === "automated" && Boolean(routine));
        return statusMatches && (!query || searchable.includes(query));
      }).slice().sort((left, right) => String(left.name || "").localeCompare(String(right.name || "")));
    },
    profileFiltersActive() { return Boolean(this.profileFilter.trim()) || this.profileFilterStatus !== "all"; },
    profileFilterSummary() { return `${this.filteredProfiles.length} of ${this.profiles.length} profile${this.profiles.length === 1 ? "" : "s"}`; },
    enabledRoutines() { return this.routines.filter((routine) => routine.enabled === true); },
    realtimeRoutines() { return this.enabledRoutines.filter((routine) => routine.mode === "realtime"); },
    realtimeDetail() { const fallbacks = this.realtimeRoutines.filter((routine) => String(routine.backend || "").includes("poll")).length; return !this.realtimeRoutines.length ? "No enabled realtime routine" : (fallbacks ? `${fallbacks} using polling fallback` : "Native/fallback backend reported healthy"); },
    readyProfileCount() { return this.profiles.filter((profile) => profile.has_password === true).length; },
    capabilities() { return this.snapshot && this.snapshot.capabilities && typeof this.snapshot.capabilities === "object" ? this.snapshot.capabilities : {}; },
    securityPolicy() { return normalizedSecurityPolicy(this.snapshot && this.snapshot.security_policy); },
    installedPackageVersion() { return boundedText(this.snapshot && this.snapshot.package && this.snapshot.package.version, "Not reported by package API"); },
    canMutate() { return this.capabilities.mutations === true && Boolean(this.csrfToken); },
    profileOutcomeUnresolved() { return scopeMutationOutcomeUnresolved(this, "profile"); },
    routineOutcomeUnresolved() { return scopeMutationOutcomeUnresolved(this, "routine"); },
    alertsOutcomeUnresolved() { return scopeMutationOutcomeUnresolved(this, "alerts"); },
    securityOutcomeUnresolved() { return scopeMutationOutcomeUnresolved(this, "security"); },
    interfaceOutcomeUnresolved() { return scopeMutationOutcomeUnresolved(this, "interface"); },
    connectionOutcomeUnresolved() { return isolatedIncidentUnresolved(this, "connection"); },
    operationOutcomeUnresolved() { return isolatedIncidentUnresolved(this, "operations"); },
    incidentOutcomeUnresolved() { return hasAnyUnresolvedIncident(this); },
    incidentGuidance() { return unresolvedIncidentGuidance(this); },
    incidentProbeTargets() { return probeableIncidents(this); },
    canCheckIncidentOutcomes() { return Boolean(this.incidentProbeTargets.length) && this.incidentProbe.active !== true; },
    // The running account: how many times, when, and what the last answer was.
    // Rendered in its own polite live region so a per-tick counter never
    // interrupts the assertive barrier alert beside it.
    incidentProbeAccount() {
      if (!this.incidentProbeTargets.length) return "";
      const probe = this.incidentProbe;
      if (!probe.attempts) return probe.active ? "Checking with DSM now…" : "Preparing to check the preserved request with DSM…";
      // `accepted` means the package told us the job is still running. Until
      // this change that was the end of the account, and a job running for forty
      // minutes said exactly what a job running for four seconds said. A phase
      // answers it when the package published one; the controller-liveness join
      // answers it when the package published nothing, which is the case that
      // used to leave an operator watching a counter increment against a
      // controller that had already stopped.
      const running = probe.verdict === "accepted";
      return guidanceText(
        `${probe.active ? "Checking now" : "Still reconciling"} · checked ${probe.attempts} time${probe.attempts === 1 ? "" : "s"} · last at ${formatDate(probe.checkedAt)}.`,
        probe.message,
        running
          ? (progressSentence(probe.progress, packageEpoch(this)) || queuedWaitText(this))
          : progressSentence(probe.progress, packageEpoch(this)),
        probe.jobId ? `Queued job ID: ${probe.jobId}.` : ""
      );
    },
    incidentScopeAvailability() {
      const unresolved = (scope) => (scope === "connection" || scope === "operations")
        ? isolatedIncidentUnresolved(this, scope)
        : scopeMutationOutcomeUnresolved(this, scope);
      const blocked = [];
      const available = [];
      INCIDENT_GATES.forEach(([label, scopes]) => {
        (scopes.some(unresolved) ? blocked : available).push(label);
      });
      return { blocked: blocked.join(", "), available: available.join(", ") };
    },
    profileOutcomeGuidance() { return scopeMutationGuidance(this, "profile"); },
    routineOutcomeGuidance() { return scopeMutationGuidance(this, "routine"); },
    alertsOutcomeGuidance() { return scopeMutationGuidance(this, "alerts"); },
    securityOutcomeGuidance() { return scopeMutationGuidance(this, "security"); },
    interfaceOutcomeGuidance() { return scopeMutationGuidance(this, "interface"); },
    connectionOutcomeGuidance() { return isolatedIncidentGuidance(this, "connection"); },
    profileDraftRecoveryGuidance() { return this.profileOutcomeUnresolved ? this.profileOutcomeGuidance : this.connectionOutcomeGuidance; },
    operationOutcomeGuidance() { return isolatedIncidentGuidance(this, "operations"); },
    profileConnectionBlocked() { return this.profileOutcomeUnresolved; },
    profileConnectionBlockedGuidance() { return this.profileOutcomeGuidance; },
    profileConnectionActionGuidance() { return this.profileOutcomeUnresolved ? this.profileOutcomeGuidance : (this.connectionOutcomeUnresolved ? this.connectionOutcomeGuidance : ""); },
    routineMutationBlocked() { return this.profileOutcomeUnresolved || this.routineOutcomeUnresolved; },
    routineMutationGuidance() { return this.profileOutcomeUnresolved ? this.profileOutcomeGuidance : this.routineOutcomeGuidance; },
    operationMutationGuidance() { return this.profileOutcomeUnresolved ? this.profileOutcomeGuidance : (this.operationOutcomeUnresolved ? this.operationOutcomeGuidance : ""); },
    connectionIncidentEvidence() { const incident = this.isolatedIncidents && this.isolatedIncidents.connection; return unresolvedIsolatedIncident(incident) ? boundedText(incident.message, "") : ""; },
    profileRecoveryActive() { return this.profileEditorOpen === true && (this.profileOutcomeUnresolved || this.connectionOutcomeUnresolved) && this.profileSaveState !== "saving" && this.profileConnectionState !== "testing"; },
    profileReconciliationIncident() {
      const incident = this.autosaveIncidents && this.autosaveIncidents.profile;
      if (!incident || incident.active !== true || !validatedClientRequestId(incident.requestId)) return null;
      if (![ACTIONS.configureProfile, ACTIONS.setSecret].includes(incident.operation)) return null;
      if (incident.operation === ACTIONS.configureProfile
        && (!incident.expectedConfiguration || typeof incident.expectedConfiguration !== "object")) return null;
      if (incident.operation === ACTIONS.setSecret && !PROFILE_SECRET_KINDS.includes(incident.secretKind)) return null;
      return incident;
    },
    connectionReconciliationIncident() {
      const incident = this.isolatedIncidents && this.isolatedIncidents.connection;
      if (!unresolvedIsolatedIncident(incident) || !validatedClientRequestId(incident.requestId)) return null;
      if (![ACTIONS.testProfileAuth, ACTIONS.browseRemote].includes(incident.operation)) return null;
      return incident;
    },
    canReconcileProfileIncident() {
      return Boolean(
        this.profileReconciliationIncident
        && this.hasCapability("request_reconciliation")
        && !this.operationBusy
        && this.profileReconciliationState !== "checking"
      );
    },
    canReconcileConnectionIncident() {
      return Boolean(
        this.connectionReconciliationIncident
        && this.hasCapability("request_reconciliation")
        && !this.operationBusy
        && this.profileReconciliationState !== "checking"
      );
    },
    snapshotRefreshBlocked() { return this.profileSaveState === "saving" || this.profileConnectionState === "testing" || (this.profileEditorOpen === true && !this.profileRecoveryActive); },
    snapshotRefreshTooltip() { return this.snapshotRefreshBlocked ? "Close the profile editor before refreshing package status" : (this.profileRecoveryActive ? "Read fresh package evidence without overwriting the preserved profile or secret draft" : "Refresh current data"); },
    canChangeInterface() { return this.canMutate && !this.operationBusy && this.securityPolicy.allow_interface_changes !== false; },
    canChangeProfiles() { return this.canMutate && !this.operationBusy && !this.profileOutcomeUnresolved && !this.connectionOutcomeUnresolved && this.securityPolicy.allow_profile_changes !== false; },
    canManageSecrets() { return this.canMutate && !this.operationBusy && !this.profileOutcomeUnresolved && !this.connectionOutcomeUnresolved && this.capabilities.secrets === true && this.securityPolicy.allow_secret_changes !== false; },
    canTestProfileAuthentication() { return this.profileEditorOpen && this.canMutate && !this.operationBusy && !this.profileOutcomeUnresolved && !this.connectionOutcomeUnresolved && this.capabilities.profile_connection_test === true && this.securityPolicy.allow_operational_actions !== false && this.profileConnectionState !== "testing" && this.profileSaveState !== "saving"; },
    connectionTestReady() { return this.profileConnectionState === "success" && Boolean(this.connectionProof) && this.connectionProofExpires > 0; },
    canSubmitProfile() { return this.profileEditorOpen && this.canChangeProfiles && !this.profileOutcomeUnresolved && this.profileSaveState !== "saving" && this.profileConnectionState !== "testing" && !(this.pathBrowser.visible && this.pathBrowser.kind === "remote" && this.pathBrowser.loading); },
    canSubmitProfileSecrets() { return Boolean(this.selectedProfile) && this.canManageSecrets && this.hasPendingSecretOperations && !this.profileOutcomeUnresolved && this.profileSaveState !== "saving" && this.profileConnectionState !== "testing" && !(this.pathBrowser.visible && this.pathBrowser.kind === "remote" && this.pathBrowser.loading); },
    canRemoveProfile() { return Boolean(this.selectedProfile) && this.canChangeProfiles && !this.profileOutcomeUnresolved && this.profileConnectionState !== "testing" && !(this.pathBrowser.visible && this.pathBrowser.kind === "remote" && this.pathBrowser.loading); },
    canSubmitRoutine() { return this.canChangeRoutines && Boolean(this.routineForm.profile) && !this.profileOutcomeUnresolved && !this.routineOutcomeUnresolved; },
    canRemoveRoutine() { return this.canChangeRoutines && Boolean(this.selectedRoutine) && !this.profileOutcomeUnresolved && !this.routineOutcomeUnresolved; },
    canSubmitAlerts() { return this.canChangeNotifications && !this.alertsOutcomeUnresolved; },
    canSubmitNotificationPreferences() { return this.canChangeNotifications && !this.interfaceOutcomeUnresolved; },
    canSubmitSecurity() { return this.canMutate && this.securityDirty && !this.operationBusy && !this.securityOutcomeUnresolved; },
    canSubmitInterface() { return this.canChangeInterface && !this.interfaceOutcomeUnresolved; },
    profileSaveButtonText() { return this.profileSaveState === "saving" ? (this.selectedProfile ? "Saving profile…" : "Creating profile…") : (this.profileOutcomeUnresolved ? "Save locked" : (this.selectedProfile ? "Save now" : "Create profile")); },
    profileOperationInProgress() {
      return this.profileEditorOpen === true
        && (this.profileSaveState === "saving"
          || this.profileConnectionState === "testing"
          || this.profileReconciliationState === "checking");
    },
    profileWindowGuardActive() {
      return this.profileEditorOpen === true
        && (this.profileOperationInProgress
          || this.profileOutcomeUnresolved
          || this.connectionOutcomeUnresolved);
    },
    profileLiveOperation() {
      if (this.disposed || !this.profileEditorOpen) return null;
      if (this.profileConnectionState === "testing") {
        return {
          title: "Testing profile authentication",
          message: boundedText(this.profileConnectionMessage, "Discovering File Station and testing this connection draft…")
        };
      }
      if (this.profileReconciliationState === "checking") {
        return {
          title: "Reconciling profile request",
          message: boundedText(this.profileSaveMessage, "Resolving the preserved request against the authenticated package queue…")
        };
      }
      if (this.profileSaveState === "saving" && !this.selectedProfile) {
        const progress = this.profileCreationProgress && this.profileCreationProgress.active === true
          ? this.profileCreationProgress
          : null;
        return {
          title: "Creating profile",
          stage: progress ? `Step ${progress.current} of ${progress.total}` : "",
          message: progress
            ? boundedText(progress.message, "Validating and creating the profile…")
            : boundedText(this.profileSaveMessage, "Validating and creating the profile…"),
          warning: PROFILE_CREATION_WINDOW_WARNING
        };
      }
      return null;
    },
    canAllowHttp() { return this.canChangeProfiles && this.securityPolicy.allow_http_targets !== false; },
    canAllowEmptySource() { return this.canChangeProfiles && this.securityPolicy.allow_empty_source !== false; },
    canAllowInvalidTls() { return this.canChangeProfiles && this.securityPolicy.allow_invalid_tls !== false; },
    canAllowDestructive() { return this.canMutate && this.securityPolicy.allow_destructive_sync !== false; },
    canAllowRemoteLogging() { return this.canChangeProfiles && this.securityPolicy.allow_remote_logging !== false; },
    canReplaceRemoteLogToken() { return this.canManageSecrets && this.securityPolicy.allow_remote_logging !== false; },
    canEditHttpException() { return this.canChangeProfiles && (this.securityPolicy.allow_http_targets !== false || this.profileForm.allow_http === true); },
    canEditEmptySourceException() { return this.canChangeProfiles && (this.securityPolicy.allow_empty_source !== false || this.profileForm.allow_empty_source === true); },
    canEditInvalidTlsException() { return this.canChangeProfiles && (this.securityPolicy.allow_invalid_tls !== false || this.profileForm.danger_invalid_certs === true); },
    canEditProfileDeletion() { return this.canChangeProfiles && (this.securityPolicy.allow_destructive_sync !== false || this.profileForm.delete === true); },
    canEditRoutineDeletion() { return this.canChangeRoutines && (this.securityPolicy.allow_destructive_sync !== false || this.routineForm.allow_delete === true); },
    canEditRemoteLogging() { return this.canChangeProfiles && (this.securityPolicy.allow_remote_logging !== false || Boolean(this.profileForm.remote_log_url)); },
    canChangeRoutines() { return this.canMutate && !this.operationBusy && !this.profileOutcomeUnresolved && this.securityPolicy.allow_routine_changes !== false; },
    canChangeNotifications() { return this.canMutate && !this.operationBusy && this.securityPolicy.allow_notification_changes !== false; },
    canRunOperations() { return this.canMutate && !this.operationBusy && !this.profileOutcomeUnresolved && !this.operationOutcomeUnresolved && this.securityPolicy.allow_operational_actions !== false; },
    canRunDoctorWrite() { return this.securityPolicy.allow_doctor_write_test !== false; },
    selectedProfileModel() { return this.profiles.find((profile) => String(profile.name) === String(this.selectedProfile)) || null; },
    profileLogFile() { return boundedText(this.selectedProfileModel && this.selectedProfileModel.log_file, "Package-managed sync.log"); },
    hasPendingSecretOperations() { return Object.keys(this.secretModes).some((field) => this.secretModes[field] !== "keep"); },
    selectedRoutine() { return this.routines.find((routine) => String(routine.profile) === String(this.routineForm.profile)) || null; },
    dependencyProfiles() { return this.profiles.filter((profile) => String(profile.name) !== String(this.routineForm.profile)); },
    profileOptions() { return options([["", "Choose a profile"], ...this.profiles.map((profile) => [String(profile.name), String(profile.name)])]); },
    scopeOptions() { return options([["all", "All profiles"], ...this.profiles.map((profile) => [String(profile.name), String(profile.name)])]); },
    syncProfileOptions() { return options(this.profiles.map((profile) => [String(profile.name), String(profile.name)])); },
    syncDefaultProfile() {
      const preferred = this.profiles.find((profile) => profile.is_default === true || profile.default === true);
      return boundedText((preferred || this.profiles[0] || {}).name, "");
    },
    syncStateOptions() { return options(SYNC_STATE_OPTIONS); },
    syncPageSizeOptions() { return options(SYNC_PAGE_SIZES.map((size) => [size, `${size} rows per page`])); },
    syncLocked() { return !this.canRunOperations || this.syncStatusBusy || this.resyncBusy; },
    syncTextFields() {
      return [
        { key: "scope", help: "sync-scope", label: "Folder or file", max: 4096, placeholder: "reports/q3 — empty means the whole tree" },
        { key: "filter", help: "sync-filter", label: "Search file name", max: 128, placeholder: "invoice" }
      ];
    },
    syncSelectFields() {
      return [
        { key: "profile", help: "sync-profile", label: "Profile", options: this.syncProfileOptions, resets: true },
        { key: "state", help: "sync-state", label: "Show states", options: this.syncStateOptions },
        { key: "limit", help: "sync-limit", label: "Page size", options: this.syncPageSizeOptions }
      ];
    },
    // A truncated walk makes every figure a floor, so the "+" is attached to
    // each number where it is read rather than explained once underneath, where
    // a reader may never reach it.
    statusRollupCards() {
      const total = this.statusRollup.total;
      if (!total) return [];
      const more = this.statusRollup.complete ? "" : "+";
      return [
        { id: "in-sync", label: "In sync", value: `${total.inSync.files}${more}`, detail: formatBytes(total.inSync.bytes) },
        { id: "differs", label: "Pending upload", value: `${total.wouldTransfer.files}${more}`, detail: formatBytes(total.wouldTransfer.bytes) },
        { id: "type-conflict", label: "Needs attention", value: `${total.attention}${more}`, detail: `of ${total.totalEntries}${more} entries` }
      ];
    },
    statusRollupRows() {
      return this.statusRollup.profiles.map((entry) => {
        const more = entry.complete ? "" : "+";
        return {
          profile: entry.profile,
          inSync: `${entry.state.inSync.files}${more} · ${formatBytes(entry.state.inSync.bytes)}${more}`,
          pending: `${entry.state.wouldTransfer.files}${more} · ${formatBytes(entry.state.wouldTransfer.bytes)}${more}`,
          attention: `${entry.state.attention}${more}`,
          observed: describeEvidenceAge(entry.observedAtEpoch),
          observedExact: entry.observedAtEpoch ? formatDate(entry.observedAtEpoch) : "Never observed"
        };
      });
    },
    // The oldest contributing observation, never the newest: the newest would
    // describe the freshest part of the answer while implying it of all of it.
    statusRollupFreshness() {
      if (!this.statusRollup.loaded) return "Not read yet";
      if (!this.statusRollup.observedAtEpoch) return "Never observed";
      return `Oldest evidence ${describeEvidenceAge(this.statusRollup.observedAtEpoch)}`;
    },
    statusRollupTotalNote() {
      const rollup = this.statusRollup;
      if (!rollup.loaded) return "";
      if (!rollup.total) {
        // Rendered, never omitted. A missing row invites a reader to add the
        // per-profile figures themselves and reach the wrong answer the core
        // declined to state.
        const reason = rollup.totalUnavailableReason
          || (rollup.overlapping
            ? "Two profiles cover overlapping trees, so a combined figure would count the shared files twice."
            : "A combined figure cannot be stated over these profiles.");
        return `No combined total: ${reason} The per-profile rows below are each exact.`;
      }
      if (!rollup.complete) {
        return "A scan budget stopped at least one walk, so every figure here is a lower bound rather than a count.";
      }
      return `Combined across ${rollup.profilesObserved} of ${rollup.profilesTotal} configured profile${rollup.profilesTotal === 1 ? "" : "s"}.`;
    },
    statusRollupNeverObservedNote() {
      const names = this.statusRollup.neverObserved;
      if (!names.length) return "";
      return `Never observed: ${names.join(", ")}. Until each is checked once, the combined figure is incomplete and understates what is pending.`;
    },
    syncStatusCards() {
      const stats = this.syncStatusResult.stats;
      if (!stats) return [];
      return SYNC_STAT_CARDS.map((card) => ({ id: card[0], label: card[1], value: String(numberOr(stats[card[2]], 0)) }));
    },
    // Files synced and data moved, stated once and plainly, because they are
    // the two numbers the page exists to answer.
    syncStatusTotalsNote() {
      const stats = this.syncStatusResult.stats;
      if (!stats) return "";
      const bound = this.syncStatusResult.complete ? "" : " The walk did not finish, so these are a lower bound.";
      return `${formatBytes(numberOr(stats.in_sync_bytes, 0))} is already in sync and ${formatBytes(numberOr(stats.transfer_bytes, 0))} would be uploaded. These cover ${this.syncStatusScopeLabel} in full, not the rows below.${bound}`;
    },
    syncStatusScopeLabel() {
      const scope = boundedText(this.syncStatusResult.scope, "").trim();
      return scope && scope !== "." ? scope : "the whole source tree";
    },
    // Totals cover the scope; the table shows one page of it. Saying so in the
    // window is the difference between an honest count and a misleading one.
    syncStatusPageSummary() {
      const stats = this.syncStatusResult.stats;
      if (!stats) return "";
      const rows = this.syncStatusResult.entries.length;
      const total = numberOr(stats.total_entries, 0);
      return `Page ${this.syncStatusPageNumber} · ${rows} row${rows === 1 ? "" : "s"} of ${total} entr${total === 1 ? "y" : "ies"} in ${this.syncStatusScopeLabel}${this.syncStatusResult.truncated ? " · more pages" : " · last page"}`;
    },
    // Names the query that produced the rows on screen, so an edited but
    // unsubmitted form can never be mistaken for what is displayed.
    syncStatusQueryLabel() {
      const query = this.syncStatusResult.query;
      if (!query) return "";
      const search = query.filter ? ` · matching "${query.filter}"` : "";
      return `${SYNC_STATE_LABELS[query.state] || query.state} in ${query.scope || "the whole source tree"}${search}`;
    },
    syncStatusHasPrevious() { return this.syncStatusCursors.length > 1; },
    syncStatusHasNext() { return this.syncStatusResult.truncated && Boolean(this.syncStatusResult.nextCursor); },
    syncStatusReady() { return Boolean(this.syncStatusForm.profile) && !this.syncLocked; },
    syncStatusFiltersActive() { return Boolean(boundedText(this.syncStatusForm.filter, "").trim()) || Boolean(boundedText(this.syncStatusForm.scope, "").trim()) || this.syncStatusForm.state !== "attention" || this.syncStatusForm.include_excluded === true; },
    resyncScopeLabel() {
      const scope = boundedText(this.resyncPlan.scope, "").trim();
      return scope && scope !== "." ? scope : "everything in this profile";
    },
    resyncPlanReady() { return this.resyncPhase === "planned" && Boolean(this.resyncPlan.ticket) && !this.resyncBusy; },
    resyncCanPlan() { return Boolean(this.syncStatusForm.profile) && !this.syncLocked; },
    resyncPlanning() { return this.resyncBusy && this.resyncPhase === "planning"; },
    resyncConfirming() { return this.resyncBusy && this.resyncPhase === "confirming"; },
    resyncFailed() { return this.resyncPhase === "failed" || this.resyncPhase === "unknown"; },
    doctorLevelOptions() { return options([["quick", "Quick — unauthenticated negotiation"], ["standard", "Standard — complete readiness (recommended)"], ["extensive", "Extensive — deep read-only target inspection"]]); },
    doctorLevelTitle() { return { quick: "Quick · lowest target load", standard: "Standard · complete readiness", extensive: "Extensive · deepest read-only inspection" }[normalizedDoctorLevel(this.doctorForm.level)]; },
    doctorLevelGuidance() { return {
      quick: "Checks routing, TLS negotiation, and DSM API discovery without sending credentials or opening a target session.",
      standard: "Authenticates and checks File Station capabilities, then performs bounded inventory. With a configured destination, it verifies permission and samples direct children; without one, it skips permission and samples visible shared-folder roots without selecting or traversing a share. This is the balanced default.",
      extensive: "Deepens the same authenticated, read-only target checks and performs the same bounded inventory branch. A configured destination gets permission and direct-child checks; without one, permission is skipped and only visible shared-folder roots are sampled, without selecting or traversing a share. No sample renders more than five entries."
    }[normalizedDoctorLevel(this.doctorForm.level)]; },
    doctorProgressStages() {
      return [
        { id: "validation", label: "Safety and request validation", state: "ok" },
        { id: "execution", label: "Controller and target checks", state: this.doctorProgress.active ? "running" : "pending" },
        { id: "evidence", label: "Terminal evidence breakdown", state: "pending" }
      ];
    },
    doctorSummaryCards() {
      const summary = this.doctorReport && this.doctorReport.summary ? this.doctorReport.summary : doctorSummary([]);
      return [
        { state: "ok", label: "OK", count: Number(summary.ok) || 0 },
        { state: "warn", label: "Warnings", count: Number(summary.warn) || 0 },
        { state: "failed", label: "Not OK", count: Number(summary.failed) || 0 },
        { state: "skipped", label: "Skipped", count: Number(summary.skipped) || 0 }
      ];
    },
    doctorReportNote() {
      if (!this.doctorReport || !this.doctorReport.sections.length) return "No diagnostic has completed in this AppWindow session.";
      if (this.doctorReport.output_incomplete) return "Terminal NDJSON was incomplete or truncated. Displayed sections are retained only as partial troubleshooting evidence and are not a complete Doctor result.";
      if (!this.doctorReport.structured) return "This installed package returned legacy terminal text. Upgrade to a structured Doctor build for authoritative per-area evidence.";
      if (this.doctorReport.summary.pending || this.doctorReport.summary.running) return "Checks are still in progress; pending areas are not counted as successful.";
      return "Each status below is based on returned target evidence; missing areas are marked skipped rather than assumed healthy.";
    },
    doctorCopyAvailable() { return Boolean(this.doctorReport && this.doctorReport.sections && this.doctorReport.sections.length); },
    // The denominator the CLI prints is its own section count. Placeholders the
    // AppWindow synthesises for a document that omitted a section carry no step,
    // so counting the steps that arrived keeps "step N of T" agreeing with the
    // run rather than with whatever the view padded it to.
    doctorStepTotal() {
      return this.doctorReport.sections.filter((section) => section.step).length;
    },
    doctorCleanupWarning() {
      const section = this.doctorReport && Array.isArray(this.doctorReport.sections)
        ? this.doctorReport.sections.find((item) => item.id === "disposable_write_verify_cleanup" && ["warn", "failed"].includes(doctorState(item.state, "warn")))
        : null;
      return section ? `${section.profile ? `Profile ${section.profile}: ` : ""}${section.detail} Inspect the named target before running another write probe.` : "";
    },
    run() { return this.snapshot && this.snapshot.run && typeof this.snapshot.run === "object" ? this.snapshot.run : ((this.snapshot && this.snapshot.last_run) || {}); },
    runStatus() { return boundedText(pick(this.run, "status", "state", "result"), "Unavailable"); },
    runScope() { return boundedText(this.run.scope, "Unavailable"); },
    runOperation() { return boundedText(this.run.operation, "Unavailable"); },
    lastRunDetail() { return this.run.finished_epoch ? formatDate(this.run.finished_epoch) : "No completion time"; },
    serviceState() { const service = this.snapshot && this.snapshot.service; const value = service && typeof service === "object" ? pick(service, "state", "status") : service; return boundedText(value, this.snapshot ? "unknown" : "Unavailable"); },
    overviewSummary() { return this.serviceState === "running" ? "The package controller is running. Status and logs update while this window remains open." : `The package controller reports ${this.serviceState}. Review Health and Activity before relying on automation.`; },
    // Everything this reads has been on the wire all along: the snapshot has
    // published `controller.state`, `active_pid` and `updated_epoch` since the
    // controller learned to persist them, and this window has been reading
    // `service` for the Overview sentence above. What was missing was the
    // question. Nothing ever correlated controller liveness with a job that is
    // sitting in the queue, so a request waiting behind a four-hour sync and a
    // request waiting behind a controller that died an hour ago both rendered as
    // the single word "pending" -- and those two call for opposite actions.
    controllerLiveness() {
      const controller = this.snapshot && this.snapshot.controller;
      const record = controller && typeof controller === "object" && !Array.isArray(controller) ? controller : null;
      // The package's clock, never the browser's: `updated_epoch` is written by
      // the NAS, so only another NAS timestamp can be subtracted from it without
      // turning clock skew into a liveness verdict.
      const generatedEpoch = numberOr(this.snapshot && this.snapshot.generated_at_epoch, 0);
      const updatedEpoch = numberOr(record && record.updated_epoch, 0);
      return {
        known: Boolean(this.snapshot && record),
        packageEpoch: generatedEpoch,
        // The live PID check rather than the controller's own last self-report.
        // A controller killed outright leaves `state=running` behind in its
        // state file, and preferring that record over the process table is how a
        // dead daemon goes on describing itself as healthy.
        service: this.serviceState,
        activePid: numberOr(record && record.active_pid, 0),
        updatedEpoch,
        // Only meaningful while nothing is active. A controller executing a long
        // job publishes its active PID and then blocks for however long that job
        // takes, so reading staleness there would have every slow sync report
        // its own controller as dead.
        stale: generatedEpoch > 0 && updatedEpoch > 0
          && generatedEpoch - updatedEpoch >= CONTROLLER_TICK_STALE_SECONDS
      };
    },
    // Why a queued job has not started yet. "Pending" is true of all of these
    // and useful for none of them.
    //
    // The reason only, with no "this is queued" lead-in: every caller but one is
    // somewhere that has already said so -- the queued toast opens with the
    // package's own "still working through its queue", and the reconciliation
    // barrier with "its job is still running". `liveProgressDetail` is the one
    // that needs the lead-in, and adds it.
    queuedWaitDetail() {
      const liveness = this.controllerLiveness;
      if (!liveness.known) {
        return {
          kind: "unknown",
          text: "The package controller state is unavailable in this session, so what this is waiting on cannot be shown here. Review Health and Activity."
        };
      }
      if (liveness.service === "stopped") {
        return {
          kind: "stopped",
          text: guidanceText(
            `The package controller is stopped${liveness.updatedEpoch ? ` (since ${formatDate(liveness.updatedEpoch)})` : ""}.`,
            "The request is preserved and runs when the package is started in Package Center."
          )
        };
      }
      if (liveness.service === "untrusted") {
        return {
          kind: "untrusted",
          text: "A process holds the package controller's PID file and is not the controller, so nothing is servicing the queue. Restart the package in Package Center, then review Logs."
        };
      }
      if (liveness.service !== "running") {
        return {
          kind: "unknown",
          text: `The package controller reports ${liveness.service}, so when this will start cannot be established. Review Health and Activity.`
        };
      }
      if (liveness.activePid > 0) {
        const operation = boundedText(this.run.operation, "");
        const scope = boundedText(this.run.scope, "");
        const named = this.runStatus === "running" && operation && operation !== "none";
        const behind = named
          ? `the ${operation}${scope && scope !== "none" ? ` of ${scope}` : ""}${numberOr(this.run.started_epoch, 0) ? `, started ${formatDate(this.run.started_epoch)}` : ""}`
          : "another operation the controller has already started";
        return { kind: "behind", text: `The controller is already running ${behind}, so this starts as soon as that finishes.` };
      }
      if (liveness.stale) {
        return {
          kind: "stalled",
          text: guidanceText(
            `The package controller last reported at ${formatDate(liveness.updatedEpoch)} and has not checked in since, although it is running nothing.`,
            "It may be wedged. Review Logs, and restart the package if it does not recover."
          )
        };
      }
      return {
        kind: "queued",
        text: "The package controller is running and starts this as soon as the work ahead of it finishes."
      };
    },
    // What the running operation last said it was doing, or -- when it has
    // published nothing -- why it has not started. Progress wins whenever there
    // is progress: a job reporting phases is plainly not waiting for one.
    liveProgressDetail() {
      if (!this.liveProgress.active) return "";
      const published = progressSentence(this.liveProgress.progress, this.controllerLiveness.packageEpoch);
      // The one caller that supplies the lead-in: a surface showing a spinner and
      // "Doctor is running" has to be told when the thing is not running at all.
      return published || guidanceText("Queued.", this.queuedWaitDetail.text);
    },
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
    compareOptions() { return options([["content", "Content — size, MD5, CRC32, SHA-256, mtime"], ["metadata", "Metadata — size and mtime"], ["size-only", "Size only"]]); },
    verbosityOptions() { return options([[0, "Normal"], [1, "Verbose"], [2, "Very verbose"]]); },
    logLevelOptions() { return options(["trace", "debug", "info", "warn", "error", "off"].map((value) => [value, value])); },
    logFormatOptions() { return options([["human", "Human readable"], ["json", "Structured JSON"]]); },
    progressOptions() { return options([["auto", "Automatic"], ["always", "Always"], ["never", "Never"]]); },
    outputOptions() { return options([["human", "Human readable"], ["json", "JSON"], ["ndjson", "Newline-delimited JSON"]]); },
    remoteLogModeOptions() { return options([["best-effort", "Best effort"], ["required", "Required"]]); },
    secretModeOptions() { return options([["keep", "Keep existing"], ["replace", "Replace securely"], ["clear", "Clear stored value"]]); },
    profileFilterStatusOptions() { return options([["all", "All profiles"], ["ready", "Password stored"], ["needs-password", "Needs password"], ["default", "Default profile"], ["automated", "Has a routine"]]); },
    routineActionOptions() { return options([["sync", "Sync"], ["plan", "Plan only"]]); },
    routineModeOptions() { return options([["interval", "Interval"], ["daily", "Daily window"], ["realtime", "Realtime watcher"]]); },
    weekdayOptions() { return options([[1, "Mon"], [2, "Tue"], [3, "Wed"], [4, "Thu"], [5, "Fri"], [6, "Sat"], [7, "Sun"]]); },
    logSourceOptions() { return options([["all", "All logs"], ["api", "DSM API"], ["doctor", "Doctor discovery"], ["audit", "Audit"], ["controller", "Controller"], ["scheduler", "Scheduler"], ["sync", "Sync"]]); },
    activityCategoryOptions() { return options([["all", "All categories"], ["audit", "Audit"], ["bridge", "Bridge"], ["authentication", "Authentication"], ["security", "Security"], ["configuration", "Configuration"], ["secrets", "Secrets"], ["routines", "Routines"], ["operations", "Operations"], ["notifications", "Notifications"], ["sync", "Sync"], ["controller", "Controller"], ["scheduler", "Scheduler"]]); },
    activityLevelOptions() { return options([["all", "All levels"], ...["trace", "debug", "info", "warn", "error"].map((level) => [level, level])]); },
    logLineOptions() { return options([[100, "100 lines"], [200, "200 lines"], [500, "500 lines"], [1000, "1000 lines"]]); },
    themeOptions() { return options([["dark", "Hellfire dark"], ["system", "Follow system"], ["light", "Ash light"]]); },
    statusRefreshOptions() { return options([[0, "Manual only"], [1000, "Every second"], [3000, "Every 3 seconds"], [5000, "Every 5 seconds"], [10000, "Every 10 seconds"], [30000, "Every 30 seconds"]]); },
    logRefreshOptions() { return options([[0, "Manual only"], [5000, "Every 5 seconds"], [10000, "Every 10 seconds"], [30000, "Every 30 seconds"]]); }
  },
  watch: {
    profileForm: { deep: true, handler() { this.autosaveChanged("profile"); } },
    "profileForm.url"() { this.invalidateConnectionTest(); },
    "profileForm.username"() { this.invalidateConnectionTest(); },
    "profileForm.allow_http"() { this.invalidateConnectionTest(); },
    "profileForm.danger_invalid_certs"() { this.invalidateConnectionTest(); },
    "profileForm.ca_certificate"() { this.invalidateConnectionTest(); },
    "profileForm.connect_timeout"() { this.invalidateConnectionTest(); },
    "profileForm.timeout"() { this.invalidateConnectionTest(); },
    "profileForm.retries"() { this.invalidateConnectionTest(); },
    routineForm: { deep: true, handler() { this.autosaveChanged("routine"); } },
    alertForm: { deep: true, handler() { this.autosaveChanged("alerts"); } },
    securityForm: { deep: true, handler() { this.autosaveChanged("security"); } },
    settings: { deep: true, handler() { this.autosaveChanged("interface"); } },
    operationBusy(value) { if (this.autosaveCoordinator) this.autosaveCoordinator.setGlobalBusy(value === true); },
    incidentOutcomeUnresolved(value) {
      if (value) this.incidentProbeStep = 0;
      else this.incidentProbe = emptyIncidentProbe();
      this.scheduleIncidentProbe();
    }
  },
  async mounted() {
    this.autosaveCoordinator = createAutosaveCoordinator({
      delayMs: 1300,
      dispatch: (task) => this.dispatchAutosave(task),
      onSuccess: (task) => this.autosaveSucceeded(task),
      onError: (error, task) => this.autosaveFailed(error, task),
      onSuperseded: () => this.refreshAutosaveStatus()
    });
    this.autosaveCoordinator.hydrate("interface", this.interfaceSettingsPayload());
    this.connectionWatchCleanups = [
      this.$watch("secretModes", () => this.invalidateConnectionTest(), { deep: true }),
      this.$watch("secretValues.password", () => this.invalidateConnectionTest()),
      this.$watch("secretValues.totp", () => this.invalidateConnectionTest())
    ];
    this.controlLayoutCleanup = installControlLayout(this.$el);
    this.abortController = typeof window.AbortController === "function" ? new window.AbortController() : null;
    this.auth = {
      signal: this.abortController ? this.abortController.signal : undefined,
      onCsrfReissued: (previousToken, replacementToken) => {
        if (!this.disposed && this.csrfToken === previousToken) this.csrfToken = replacementToken;
      }
    };
    this.mediaQuery = window.matchMedia ? window.matchMedia("(prefers-color-scheme: light)") : null;
    this.systemLight = Boolean(this.mediaQuery && this.mediaQuery.matches);
    this.mediaHandler = (event) => { this.systemLight = event.matches; };
    if (this.mediaQuery && this.mediaQuery.addEventListener) this.mediaQuery.addEventListener("change", this.mediaHandler);
    this.visibilityHandler = () => {
      if (document.hidden) {
        this.stopTimers();
        const protectedProfileDraft = this.profileEditorOpen === true
          && (this.profileSaveState === "saving"
            || this.profileConnectionState === "testing"
            || this.profileReconciliationState === "checking"
            || this.profileOutcomeUnresolved
            || this.connectionOutcomeUnresolved);
        if (!protectedProfileDraft) this.clearSecrets();
        if (this.autosaveCoordinator) this.autosaveCoordinator.setGlobalBusy(true);
      } else {
        if (this.autosaveCoordinator) this.autosaveCoordinator.setGlobalBusy(this.operationBusy);
        this.refreshSnapshot(false);
        if (this.route === "activity") this.refreshLogs();
        this.scheduleIncidentProbe();
      }
    };
    document.addEventListener("visibilitychange", this.visibilityHandler);
    this.installProfileWindowGuard();
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
    purgeReconciliationAuth(this.auth);
    if (this.autosaveCoordinator) this.autosaveCoordinator.dispose();
    if (this.abortController) this.abortController.abort();
    this.stopTimers();
    this.toastTimers.forEach((timer) => window.clearTimeout(timer));
    this.toastTimers = [];
    if (this.visibilityHandler) document.removeEventListener("visibilitychange", this.visibilityHandler);
    this.removeProfileWindowGuard();
    if (this.mediaQuery && this.mediaQuery.removeEventListener && this.mediaHandler) this.mediaQuery.removeEventListener("change", this.mediaHandler);
    if (this.controlLayoutCleanup) this.controlLayoutCleanup();
    this.connectionWatchCleanups.forEach((cleanup) => { if (typeof cleanup === "function") cleanup(); });
    this.connectionWatchCleanups = [];
    this.clearConnectionProofTimer();
    this.removeConfirmationKeyHandler();
    this.removePathBrowserKeyHandler();
    if (this.confirmation.resolve) this.confirmation.resolve(false);
    this.confirmationPriorFocus = null;
    this.pathBrowserPriorFocus = null;
    this.clearSecrets();
    this.csrfToken = "";
    this.auth = { signal: undefined };
  },
  methods: {
    formatBytes, formatDate, formatDuration,
    strictDraftInteger(value) {
      if (typeof value === "number") return Number.isSafeInteger(value) ? value : null;
      const text = String(value === null || value === undefined ? "" : value).trim();
      if (!/^-?(?:0|[1-9]\d*)$/.test(text)) return null;
      const parsed = Number(text);
      return Number.isSafeInteger(parsed) ? parsed : null;
    },
    profileAutosavePayload() {
      const jobs = this.strictDraftInteger(this.profileForm.jobs);
      const maxDelete = this.strictDraftInteger(this.profileForm.max_delete);
      const retries = this.strictDraftInteger(this.profileForm.retries);
      const timeout = this.strictDraftInteger(this.profileForm.timeout);
      const connectTimeout = this.strictDraftInteger(this.profileForm.connect_timeout);
      const maxRate = this.strictDraftInteger(this.profileForm.max_rate);
      const verbosity = this.strictDraftInteger(this.profileForm.verbosity);
      if ([jobs, maxDelete, retries, timeout, connectTimeout, maxRate, verbosity].some((value) => value === null)) return null;
      return Object.assign(this.profilePayload(), {
        jobs,
        max_delete: maxDelete,
        retries,
        timeout_seconds: timeout,
        connect_timeout_seconds: connectTimeout,
        max_rate_bytes_per_second: maxRate === 0 ? null : maxRate,
        verbosity
      });
    },
    routineAutosavePayload() {
      const retryCount = this.strictDraftInteger(this.routineForm.retry_count);
      const retryBackoff = this.strictDraftInteger(this.routineForm.retry_backoff_seconds);
      const maxDelete = this.strictDraftInteger(this.routineForm.max_total_delete);
      if ([retryCount, retryBackoff, maxDelete].some((value) => value === null)) return null;
      const payload = Object.assign(this.routinePayload(), {
        retry_count: retryCount,
        retry_backoff_seconds: retryBackoff,
        max_total_delete: maxDelete
      });
      if (payload.mode === "interval") {
        const interval = this.strictDraftInteger(this.routineForm.interval_seconds);
        if (interval === null) return null;
        payload.interval_seconds = interval;
      } else if (payload.mode === "realtime") {
        const debounce = this.strictDraftInteger(this.routineForm.debounce_seconds);
        const poll = this.strictDraftInteger(this.routineForm.poll_seconds);
        if (debounce === null || poll === null) return null;
        payload.debounce_seconds = debounce;
        payload.poll_seconds = poll;
      }
      return payload;
    },
    alertPayload() { return { enabled: this.alertForm.enabled === true, on_success: this.alertForm.on_success === true, on_failure: this.alertForm.on_failure === true, failure_threshold: this.strictDraftInteger(this.alertForm.failure_threshold), cooldown_seconds: this.strictDraftInteger(this.alertForm.cooldown_seconds) }; },
    interfaceSettingsPayload() { return { theme: this.settings.theme, status_refresh: Number(this.settings.status_refresh), log_refresh: Number(this.settings.log_refresh) }; },
    validateRoutinePayload(payload) {
      if (!payload || !payload.profile) return "Choose an existing profile.";
      if (payload.mode === "daily" && !payload.weekdays.length) return "Select at least one active weekday.";
      if (payload.allow_delete && !this.canAllowDestructive) return "The security policy does not permit deletion-capable routines.";
      if (!this.between(payload.retry_count, 0, 5) || !this.between(payload.retry_backoff_seconds, 10, 300) || !this.between(payload.max_total_delete, 0, 2147483647)) return "One or more retry or deletion limits are outside the supported range.";
      if (payload.mode === "interval" && !this.between(payload.interval_seconds, 60, 2592000)) return "Interval must be between 60 and 2592000 seconds.";
      if (payload.mode === "realtime" && (!this.between(payload.debounce_seconds, 1, 3600) || !this.between(payload.poll_seconds, 5, 3600))) return "Realtime debounce or fallback poll is outside the supported range.";
      if (payload.mode === "daily" && (!/^([01]\d|2[0-3]):[0-5]\d$/.test(payload.time_window_start) || !/^([01]\d|2[0-3]):[0-5]\d$/.test(payload.time_window_end))) return "Daily window times must use 24-hour HH:MM format.";
      return "";
    },
    validateAlertPayload(payload) {
      return payload && this.between(payload.failure_threshold, 1, 100) && this.between(payload.cooldown_seconds, 60, 2592000)
        ? ""
        : "Failure threshold or cooldown is outside the supported range.";
    },
    validateInterfacePayload(payload) {
      return payload && ["dark", "light", "system"].includes(payload.theme)
        && [0, 3000, 5000, 10000, 30000].includes(payload.status_refresh)
        && [0, 5000, 10000, 30000].includes(payload.log_refresh)
        ? ""
        : "Choose a supported theme and refresh cadence.";
    },
    profileAutosaveNeedsReview(payload) {
      const saved = this.selectedProfileModel;
      if (!saved) return true;
      return (payload.allow_http && pick(saved, "allow_http") !== true)
        || (payload.delete && pick(saved, "delete") !== true)
        || (payload.allow_empty_source && pick(saved, "allow_empty_source") !== true)
        || (payload.danger_accept_invalid_certs && pick(saved, "danger_invalid_certs", "danger_accept_invalid_certs") !== true);
    },
    routineAutosaveNeedsReview(payload) { return !this.selectedRoutine || (payload.allow_delete && this.selectedRoutine.allow_delete !== true); },
    autosaveCandidate(scope) {
      if (scope === "profile") {
        if (!this.profileEditorOpen) return { payload: null, invalid: "Profile editor is closed." };
        const payload = this.profileAutosavePayload();
        if (!payload) return { payload: null, invalid: "Finish the numeric profile value before autosave." };
        const invalid = this.validateProfile(payload, []);
        return { payload, invalid, manual: !this.selectedProfile ? "Save a new profile once before autosave takes over." : (this.profileAutosaveNeedsReview(payload) ? "Use Save now to approve destructive or TLS-risk changes." : "") };
      }
      if (scope === "routine") {
        if (!this.routineEditorOpen) return { payload: null, invalid: "Routine editor is closed." };
        const payload = this.routineAutosavePayload();
        if (!payload) return { payload: null, invalid: "Finish the numeric routine value before autosave." };
        const invalid = this.validateRoutinePayload(payload);
        return { payload, invalid, manual: !this.selectedRoutine ? "Save a new routine once before autosave takes over." : (this.routineAutosaveNeedsReview(payload) ? "Use Save now to approve routine deletion." : "") };
      }
      if (scope === "alerts") { const payload = this.alertPayload(); return { payload, invalid: this.validateAlertPayload(payload), manual: "" }; }
      if (scope === "security") { const payload = this.securityPayload(); return { payload, invalid: this.validateSecurityPayload(payload), manual: this.securityRelaxed(payload) ? "Use Save now to approve relaxed security restrictions." : "" }; }
      if (scope === "interface") { const payload = this.interfaceSettingsPayload(); return { payload, invalid: this.validateInterfacePayload(payload), manual: "" }; }
      return { payload: null, invalid: "Unsupported autosave scope." };
    },
    autosaveChanged(scope) {
      if (!this.autosaveCoordinator || this.disposed || !AUTOSAVE_SCOPES.includes(scope)) return;
      const state = this.autosaveCoordinator.getState(scope);
      if (!state.registered) return;
      const candidate = this.autosaveCandidate(scope);
      if (!candidate.payload || candidate.invalid) {
        this.autosaveCoordinator.cancel(scope);
        if (candidate.invalid && !/editor is closed/.test(candidate.invalid)) {
          this.autosavePhase = "blocked";
          this.autosaveMessage = candidate.invalid;
        }
        return;
      }
      const next = this.autosaveCoordinator.update(scope, candidate.payload);
      const failurePaused = Boolean(this.autosaveFailureScopes && this.autosaveFailureScopes[scope] === true);
      const anyFailurePaused = AUTOSAVE_SCOPES.some((candidateScope) => (
        this.autosaveFailureScopes && this.autosaveFailureScopes[candidateScope] === true
      ));
      const connectionHeld = scope === "profile" && this.profileConnectionAutosaveHeld === true;
      const profileDependencyBlocked = scope === "routine" && scopeMutationOutcomeUnresolved(this, "profile");
      this.autosaveCoordinator.setScopeBlocked(scope, scopeMutationOutcomeUnresolved(this, scope) || failurePaused || connectionHeld || profileDependencyBlocked || Boolean(candidate.manual));
      if (scope === "alerts") this.alertDirty = next.dirty;
      const status = currentAutosaveStatus(
        this.autosaveCoordinator,
        this.autosaveFailureScopes,
        "All changes saved",
        this.autosaveOutcomeUnknownScopes,
        this.autosaveInspectionScopes,
        { profile: this.profileConnectionAutosaveHeld === true }
      );
      this.autosavePhase = status.phase;
      this.autosaveMessage = status.phase === "blocked" && candidate.manual && !anyFailurePaused ? candidate.manual : status.message;
    },
    async dispatchAutosave(task) {
      if (scopeMutationOutcomeUnresolved(this, task.scope)) throw unresolvedScopeError(this, task.scope);
      if (task.scope === "routine" && scopeMutationOutcomeUnresolved(this, "profile")) {
        const deferred = unresolvedScopeError(this, "profile");
        deferred.autosaveDeferred = true;
        throw deferred;
      }
      if (this.disposed || !this.csrfToken) throw new Error("The authenticated package bridge is unavailable.");
      if (task.scope === "profile" && (this.profileConnectionState === "testing"
        || (this.pathBrowser && this.pathBrowser.visible && this.pathBrowser.kind === "remote" && this.pathBrowser.loading))) {
        const deferred = new Error("Profile autosave is held until the active connection request settles.");
        deferred.autosaveDeferred = true;
        throw deferred;
      }
      this.autosavePhase = "saving";
      this.autosaveMessage = "Saving changes…";
      this.operationBusy = true;
      try {
        const post = (action, payload) => apiPost(
          this.auth,
          this.csrfToken,
          action,
          payload,
          true,
          undefined,
          AUTOSAVE_API_LIMITS
        );
        if (task.scope === "profile") await post(ACTIONS.configureProfile, task.value);
        else if (task.scope === "routine") await post(ACTIONS.routine, task.value);
        else if (task.scope === "alerts") await post(ACTIONS.alertPolicy, task.value);
        else if (task.scope === "security") {
          await post(ACTIONS.securityPolicy, task.value);
          this.csrfToken = "";
          try {
            await this.refreshCsrf(AUTOSAVE_API_LIMITS);
          } catch (_error) {
            throw new QueuedOutcomeUnknownError(
              "",
              "DSM applied the security autosave, but refreshed request authentication is unavailable. Do not save the policy again; inspect Activity and Logs."
            );
          }
        } else if (task.scope === "interface") {
          const transaction = this.captureSettingsTransaction();
          if (!transaction) throw new Error("Browser preference storage is unavailable.");
          const next = Object.assign({}, transaction.settings, task.value);
          if (!this.persistSettings(next)) throw new Error("Browser preference storage rejected the update.");
          this.applySettingsState(next);
          try {
            await post(ACTIONS.clientEvent, { event: "interface-settings" });
          } catch (error) {
            if (this.preferenceAuditWasRejected(error)
              || (error && error.preAcceptance === true && error.clientTimeout === true)) {
              this.restoreSettingsTransaction(transaction);
            }
            throw error;
          }
          this.scheduleSnapshot();
          this.scheduleLogs();
        } else throw new Error("Unsupported autosave scope.");
        // A trusted POST acknowledgement completes autosave. Snapshot refresh
        // is follow-up observation and must not hold the mutation queue or its
        // status in "Saving" when DSM/QuickConnect reads are slow.
        if (task.scope !== "interface") void this.refreshSnapshot(false, true);
      } catch (error) {
        if (!this.disposed) this.reportMutationError(error, "Autosave failed", "Autosave outcome unknown", "The package rejected the autosave mutation.");
        throw error;
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    autosaveSucceeded(task) {
      if (task.scope === "profile") {
        this.clearProfileConfigurationFailure(false);
      } else {
        if (this.autosaveFailureScopes) this.autosaveFailureScopes[task.scope] = false;
        if (this.autosaveOutcomeUnknownScopes) this.autosaveOutcomeUnknownScopes[task.scope] = false;
        if (this.autosaveInspectionScopes) this.autosaveInspectionScopes[task.scope] = false;
        clearScopeIncident(this, task.scope);
      }
      const state = this.autosaveCoordinator ? this.autosaveCoordinator.getState(task.scope) : null;
      if (task.scope === "alerts" && state && !state.dirty) this.alertDirty = false;
      if (task.scope === "security" && state && !state.dirty) this.securityDirty = false;
      const status = currentAutosaveStatus(
        this.autosaveCoordinator,
        this.autosaveFailureScopes,
        "Changes autosaved",
        this.autosaveOutcomeUnknownScopes,
        this.autosaveInspectionScopes
      );
      this.autosavePhase = status.phase;
      this.autosaveMessage = status.message;
    },
    autosaveFailed(error, task) {
      if (error && error.autosaveDeferred === true) {
        if (this.autosaveCoordinator) this.autosaveCoordinator.setScopeBlocked(task.scope, true);
        this.refreshAutosaveStatus("Profile autosave held for the active connection request");
        return;
      }
      // Accepted, and the package told us so repeatedly: the last trusted read
      // said the job is queued. Only the browser's observation window ran out.
      //
      // Treated like the deferred case rather than the failed one. It must not
      // set `autosaveFailureScopes` or `autosaveOutcomeUnknownScopes`, because
      // both of those tell the operator to stop and inspect a change that is
      // going to apply. The scope stays blocked so a second edit cannot race
      // the outstanding one, and nothing re-sends it -- the mutation is already
      // on the queue under a known job ID.
      if (error && error.stillPending === true) {
        if (this.autosaveCoordinator) this.autosaveCoordinator.setScopeBlocked(task.scope, true);
        this.refreshAutosaveStatus("Saved changes are queued behind a running operation · they will apply when it finishes");
        return;
      }
      this.pauseAutosave(
        task.scope,
        error,
        "",
        task.scope === "profile" ? { expectedConfiguration: task.value, creatingProfile: false } : undefined
      );
    },
    hydrateAutosave(scope, payload, authoritative = true) {
      if (!this.autosaveCoordinator || !payload) return;
      const preserveFailure = authoritative !== true && Boolean(this.autosaveFailureScopes && this.autosaveFailureScopes[scope] === true);
      this.autosaveCoordinator.hydrate(scope, payload);
      if (!this.autosaveFailureScopes || typeof this.autosaveFailureScopes !== "object") {
        this.autosaveFailureScopes = { profile: false, routine: false, alerts: false, security: false, interface: false };
      }
      if (!this.autosaveOutcomeUnknownScopes || typeof this.autosaveOutcomeUnknownScopes !== "object") {
        this.autosaveOutcomeUnknownScopes = { profile: false, routine: false, alerts: false, security: false, interface: false };
      }
      if (!this.autosaveInspectionScopes || typeof this.autosaveInspectionScopes !== "object") {
        this.autosaveInspectionScopes = { profile: false, routine: false, alerts: false, security: false, interface: false };
      }
      if (scope === "profile") {
        if (authoritative === true) this.clearProfileConfigurationFailure(false);
        else this.syncProfileFailureState(false);
      } else {
        this.autosaveFailureScopes[scope] = preserveFailure;
        if (!preserveFailure) this.autosaveOutcomeUnknownScopes[scope] = false;
        if (!preserveFailure) this.autosaveInspectionScopes[scope] = false;
        if (!preserveFailure) clearScopeIncident(this, scope);
        if (preserveFailure) this.autosaveCoordinator.setScopeBlocked(scope, true);
      }
      if (scope === "alerts") this.alertDirty = false;
      if (scope === "security") this.securityDirty = false;
      const status = currentAutosaveStatus(
        this.autosaveCoordinator,
        this.autosaveFailureScopes,
        "All changes saved",
        this.autosaveOutcomeUnknownScopes,
        this.autosaveInspectionScopes
      );
      this.autosavePhase = status.phase;
      this.autosaveMessage = status.message;
    },
    refreshAutosaveStatus(savedMessage = "All changes saved") {
      const status = currentAutosaveStatus(
        this.autosaveCoordinator,
        this.autosaveFailureScopes,
        savedMessage,
        this.autosaveOutcomeUnknownScopes,
        this.autosaveInspectionScopes,
        // The scopes deliberately paused rather than failed. Passed through the
        // status function rather than assigned at the hold site, so the held
        // message stays correct across every later refresh instead of surviving
        // only until the next unrelated one overwrites it. Inlined rather than
        // factored into a method because the test harnesses copy a fixed list of
        // method names onto their context, and a new one would have to be added
        // to each of them.
        { profile: this.profileConnectionAutosaveHeld === true }
      );
      this.autosavePhase = status.phase;
      this.autosaveMessage = status.message;
      return status;
    },
    cancelAutosave(scope, refreshStatus = true) {
      if (this.autosaveCoordinator) this.autosaveCoordinator.cancel(scope);
      if (refreshStatus) this.refreshAutosaveStatus();
    },
    // Held, never cancelled.
    //
    // This used to call `cancelAutosave("profile")` first. `cancel()` latches
    // `entry.cancelled`, which both `_arm` and `_enqueue` refuse to act on and
    // which only `hydrate()` or a *further* edit ever clears -- so `release`'s
    // `setScopeBlocked(false)` could not revive it. An edit typed within the
    // 1.3 s debounce before Browse or Test was pressed stayed dirty and
    // undispatched for the life of the form, and because `currentAutosaveStatus`
    // drops cancelled scopes, the status line read "All changes saved" over it.
    // Blocking is the pause this always wanted: the entry keeps its pending
    // work, and the `setScopeBlocked(false)` in the release below re-arms and
    // drains it.
    holdProfileAutosaveForConnection() {
      this.profileConnectionAutosaveHeld = true;
      if (!this.autosaveCoordinator) return;
      const state = this.autosaveCoordinator.getState("profile");
      if (state.registered) this.autosaveCoordinator.setScopeBlocked("profile", true);
      this.refreshAutosaveStatus();
    },
    // Release runs from `finally` on both the success and the failure path, so
    // this is where the held draft's fate is decided.
    //
    // A probe that failed or whose outcome is unknown leaves an unresolved
    // connection incident, and the draft that was pending when it started must
    // not be dispatched into that. Two tests pin the absence of a profile
    // mutation after such a probe. Until now that absence held only as a side
    // effect of the hold cancelling the entry outright -- which is also what
    // lost the edit on the *success* path, where nothing was ever wrong.
    //
    // Discarding here instead, by the incident's own predicate, keeps the
    // guarantee deliberately rather than accidentally: a failed probe still
    // fires nothing, a clean probe hands the edit back, and either way a later
    // edit re-arms the scope because `update()` clears the cancelled latch.
    releaseProfileAutosaveFromConnection() {
      this.profileConnectionAutosaveHeld = false;
      if (this.disposed) return;
      if (this.autosaveCoordinator) {
        const state = this.autosaveCoordinator.getState("profile");
        if (state.registered) {
          if (isolatedIncidentUnresolved(this, "connection")) this.autosaveCoordinator.cancel("profile");
          const blocked = Boolean(this.autosaveFailureScopes && this.autosaveFailureScopes.profile === true)
            || scopeMutationOutcomeUnresolved(this, "profile");
          this.autosaveCoordinator.setScopeBlocked("profile", blocked);
        }
      }
      this.refreshAutosaveStatus();
    },
    ensureProfileFailureRecords() {
      const records = this.profileFailureRecords;
      const valid = records && typeof records === "object"
        && records.configuration && typeof records.configuration === "object"
        && records.secrets && typeof records.secrets === "object"
        && PROFILE_SECRET_KINDS.every((kind) => records.secrets[kind] && typeof records.secrets[kind] === "object");
      if (!valid) this.profileFailureRecords = emptyProfileFailureRecords();
      return this.profileFailureRecords;
    },
    syncProfileFailureState(refreshStatus = true) {
      if (!this.autosaveFailureScopes || typeof this.autosaveFailureScopes !== "object") {
        this.autosaveFailureScopes = { profile: false, routine: false, alerts: false, security: false, interface: false };
      }
      if (!this.autosaveOutcomeUnknownScopes || typeof this.autosaveOutcomeUnknownScopes !== "object") {
        this.autosaveOutcomeUnknownScopes = { profile: false, routine: false, alerts: false, security: false, interface: false };
      }
      if (!this.autosaveInspectionScopes || typeof this.autosaveInspectionScopes !== "object") {
        this.autosaveInspectionScopes = { profile: false, routine: false, alerts: false, security: false, interface: false };
      }
      const summary = profileFailureSummary(this.ensureProfileFailureRecords());
      this.autosaveFailureScopes.profile = summary.active;
      this.autosaveOutcomeUnknownScopes.profile = summary.outcomeUnknown;
      this.autosaveInspectionScopes.profile = summary.requiresInspection;
      if (!summary.active) clearScopeIncident(this, "profile");
      if (this.autosaveCoordinator) {
        const state = this.autosaveCoordinator.getState("profile");
        if (state.registered) this.autosaveCoordinator.setScopeBlocked("profile", summary.active || this.profileConnectionAutosaveHeld === true);
      }
      if (refreshStatus) this.refreshAutosaveStatus();
      return summary;
    },
    recordProfileFailure(secretKind, error) {
      const records = this.ensureProfileFailureRecords();
      const record = PROFILE_SECRET_KINDS.includes(secretKind)
        ? records.secrets[secretKind]
        : records.configuration;
      record.active = true;
      record.outcomeUnknown = record.outcomeUnknown === true || Boolean(error && error.outcomeUnknown === true);
      record.requiresInspection = record.requiresInspection === true
        || Boolean(error && (error.requiresInspection === true || error.outcomeUnknown === true));
      return this.syncProfileFailureState();
    },
    clearProfileConfigurationFailure(refreshStatus = true) {
      const record = this.ensureProfileFailureRecords().configuration;
      record.active = false;
      record.outcomeUnknown = false;
      record.requiresInspection = false;
      return this.syncProfileFailureState(refreshStatus);
    },
    clearProfileSecretFailures(secretKinds, refreshStatus = true) {
      const records = this.ensureProfileFailureRecords();
      const kinds = Array.isArray(secretKinds) ? secretKinds : [];
      for (const kind of PROFILE_SECRET_KINDS) {
        if (!kinds.includes(kind)) continue;
        records.secrets[kind].active = false;
        records.secrets[kind].outcomeUnknown = false;
        records.secrets[kind].requiresInspection = false;
      }
      return this.syncProfileFailureState(refreshStatus);
    },
    pauseAutosave(scope, error = null, profileSecretKind = "", metadata = undefined) {
      this.cancelAutosave(scope, false);
      const subject = scope === "profile"
        ? boundedText(this.selectedProfile || (this.profileForm && this.profileForm.name), "")
        : (scope === "routine" ? boundedText(this.routineForm && this.routineForm.profile, "") : INCIDENT_SCOPE_LABELS[scope]);
      const incidentMetadata = Object.assign({}, metadata && typeof metadata === "object" ? metadata : {}, {
        secretKind: profileSecretKind
      });
      recordScopeIncident(this, scope, error, subject, incidentMetadata);
      if (scope === "profile") {
        this.recordProfileFailure(profileSecretKind, error);
        return;
      }
      if (!this.autosaveFailureScopes || typeof this.autosaveFailureScopes !== "object") {
        this.autosaveFailureScopes = { profile: false, routine: false, alerts: false, security: false, interface: false };
      }
      if (!this.autosaveOutcomeUnknownScopes || typeof this.autosaveOutcomeUnknownScopes !== "object") {
        this.autosaveOutcomeUnknownScopes = { profile: false, routine: false, alerts: false, security: false, interface: false };
      }
      if (!this.autosaveInspectionScopes || typeof this.autosaveInspectionScopes !== "object") {
        this.autosaveInspectionScopes = { profile: false, routine: false, alerts: false, security: false, interface: false };
      }
      this.autosaveFailureScopes[scope] = true;
      this.autosaveOutcomeUnknownScopes[scope] = this.autosaveOutcomeUnknownScopes[scope] === true
        || Boolean(error && error.outcomeUnknown === true);
      this.autosaveInspectionScopes[scope] = this.autosaveInspectionScopes[scope] === true
        || Boolean(error && (error.requiresInspection === true || error.outcomeUnknown === true));
      if (this.autosaveCoordinator) {
        const state = this.autosaveCoordinator.getState(scope);
        if (state.registered) this.autosaveCoordinator.setScopeBlocked(scope, true);
      }
      this.refreshAutosaveStatus();
    },
    clearAutosaveFailure(scope) {
      if (scope === "profile") {
        this.profileFailureRecords = emptyProfileFailureRecords();
        this.syncProfileFailureState();
        return;
      }
      if (this.autosaveFailureScopes) this.autosaveFailureScopes[scope] = false;
      if (this.autosaveOutcomeUnknownScopes) this.autosaveOutcomeUnknownScopes[scope] = false;
      if (this.autosaveInspectionScopes) this.autosaveInspectionScopes[scope] = false;
      clearScopeIncident(this, scope);
      this.refreshAutosaveStatus();
    },
    describeBridgeError(error, phase = "status") {
      const status = Number(error && error.status) || 0;
      const code = String((error && error.code) || "").toLowerCase();
      const message = String((error && error.message) || "").toLowerCase();
      const stage = boundedText(error && error.stage, "").trim().slice(0, 128);
      const issue = (title, detail) => ({
        title,
        message: stage ? `${detail} Failure stage: ${stage}.` : detail
      });
      if ((status === 0 || status === 401) && code === "dsm_authentication_quickconnect_unsupported") {
        return issue(
          "QuickConnect relay unsupported",
          "QuickConnect relay cannot authenticate this third-party package AppWindow. Connect through the NAS LAN address, DDNS, a VPN, or a separately tested DSM custom reverse proxy, then reopen the app from the DSM desktop."
        );
      }
      if (status === 401) {
        return issue("DSM session expired", "Sign in to DSM again, then reopen this app from the DSM desktop.");
      }
      if (status === 403) {
        return issue("DSM access denied", "Use a DSM administrator account and, if HTTPS is required by policy, reopen this app over HTTPS.");
      }
      if ((status === 0 || status === 503) && code === "dsm_authentication_helper_unsafe") {
        return issue(
          "DSM authentication helper rejected",
          "Repair or reinstall the latest complete package release, then reopen this app from the DSM desktop. If the rejection continues, inspect the package API log; do not change helper ownership or permissions manually."
        );
      }
      if ((status === 0 || status === 503) && code === "dsm_authentication_helper_unavailable") {
        return issue(
          "DSM authentication helper unavailable",
          "Repair or reinstall the latest complete package release, then reopen this app from the DSM desktop. If the helper still cannot start, inspect the package API log."
        );
      }
      if ((status === 0 || status === 503) && code === "dsm_authentication_webapi_unavailable") {
        return issue(
          "DSM session validation unavailable",
          "Reopen this app from the DSM desktop. If validation still fails, inspect the package API log and confirm DSM web services are available."
        );
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
      if (status === 0 && code.includes("forbidden")) {
        return issue("DSM access denied", "Use a DSM administrator account and, if HTTPS is required by policy, reopen this app over HTTPS.");
      }
      if (status === 0 && (code === "dsm_authentication_webapi_rejected" || code.includes("unauthorized") || code.includes("authentication") || message.includes("redirect"))) {
        return issue("DSM session expired", "Sign in to DSM again, then reopen this app from the DSM desktop.");
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
    navigate(route) {
      if (!this.routes.some((item) => item.id === route)) return;
      if (this.route === "profiles" && route !== "profiles") {
        if (this.profileSaveState === "saving" || this.profileConnectionState === "testing" || this.profileReconciliationState === "checking") {
          this.toast("Profile operation in progress", "Keep this AppWindow open and wait for the active save, authentication test, or reconciliation to settle before leaving Profiles.", true);
          return;
        }
        if (!this.profileRecoveryActive) this.closeProfile();
      }
      if (this.route === "routines" && route !== "routines") this.closeRoutine();
      this.route = route;
      // Opening this page reads the stored totals and nothing else. That read
      // opens a handful of small documents the last walk left behind, so it is
      // affordable on every open. The per-file query below still walks both
      // trees and still runs only when a person asks for it.
      if (route === "sync") {
        if (!this.syncStatusForm.profile) this.syncStatusForm.profile = this.syncDefaultProfile;
        void this.refreshStatusRollup();
      }
      if (route === "activity") {
        this.refreshLogs();
        if (this.profileRecoveryActive) void this.refreshSnapshot(false, true);
      }
      else window.clearTimeout(this.logTimer);
    },
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
    doctorStatusClass(value) { return `is-${doctorState(value, "warn")}`; },
    doctorStatusLabel(value) { return { ok: "OK", warn: "Warning", failed: "Not OK", skipped: "Skipped", pending: "Pending", running: "Running" }[doctorState(value, "warn")]; },
    doctorInventoryScopeLabel(value) { return doctorInventoryScopeLabel(value); },
    doctorInventoryMetadata(entry) {
      if (!entry || typeof entry !== "object") return "Safe metadata unavailable";
      const metadata = [
        `Name ${doctorText(entry.name, "entry", 256)}`,
        `relative path ${entry.relative_path_truncated === true ? "truncated" : "complete"}`,
        `name ${entry.name_truncated === true ? "truncated" : "complete"}`,
        `mount boundary ${entry.mount_boundary === true ? "yes" : "no"}`
      ];
      if (entry.size_bytes !== null && entry.size_bytes !== undefined) metadata.push(formatBytes(entry.size_bytes));
      if (entry.modified) {
        const numeric = Number(entry.modified);
        metadata.push(`Modified ${Number.isFinite(numeric) && numeric > 0 ? formatDate(numeric) : doctorText(entry.modified, "Unavailable", 128)}`);
      }
      return metadata.join(" · ") || "Safe metadata unavailable";
    },
    onDoctorWriteTestChanged(value) {
      const enabled = value === true;
      this.doctorForm.write_test = enabled;
      this.doctorForm.write_confirm = false;
      if (enabled) this.doctorForm.level = "extensive";
    },
    copyDoctorDiagnostics() {
      return this.copyTroubleshootingText(
        doctorTroubleshootingText(this.doctorReport, this.diagnostic.title, this.diagnostic.output),
        "Target Doctor diagnostics",
        TROUBLESHOOTING_RECORD_LIMIT,
        true
      );
    },
    reportMutationError(error, failedTitle, unknownTitle, fallback, options = undefined) {
      const formatting = options && typeof options === "object" ? options : {};
      const unknown = Boolean(error && error.outcomeUnknown === true);
      const inspection = Boolean(error && error.requiresInspection === true);
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
      // Accepted and still on the queue. Handled here rather than only in
      // `autosaveFailed` because every manual caller that passes bounded
      // observation limits reaches this function too, and without a branch of
      // its own a still-queued change would fall through to the plain failure
      // wording and tell the operator their save was rejected -- which is worse
      // than the outcome-unknown report it replaced, not better. Not an error
      // toast: nothing has gone wrong and there is nothing to inspect.
      if (error && error.stillPending === true) {
        // `QueuedStillPendingError` has carried a `progress` field since it was
        // written and nothing has ever read it; this is the read. When the
        // package published a phase, name the phase. When it published nothing,
        // say why the job has not started -- the controller-liveness join
        // answers that from the snapshot this window already holds, and a job
        // waiting behind a running sync and a job waiting behind a controller
        // that is stopped are different facts calling for different actions.
        // Reporting both as the bare word "pending" is what this replaces.
        const queuedMessage = withCorrelation(
          guidanceText(
            observed,
            progressSentence(error.progress, packageEpoch(this)) || queuedWaitText(this),
            "Do not send it again; it is already queued under this job ID."
          ),
          "The package accepted this change and will apply it when the running operation finishes."
        );
        this.toast("Change queued", queuedMessage, false);
        return { unknown: false, inspection: false, stillPending: true, message: queuedMessage, requestId, jobId };
      }
      // A queued job that ended for a named structural reason: the worker was
      // killed, the request could not be classified, the result was unreadable.
      // Each of these used to reach this window as `unresolved` with the reason
      // left behind in the controller log, so the branch exists to carry the
      // cause and the next step rather than "Operation could not be completed".
      const queuedFailure = queuedFailureGuidance(error);
      if (queuedFailure && !unknown) {
        const message = withCorrelation(
          guidanceText(queuedFailure.text, formatting.queuedFailureGuidance),
          queuedFailure.text
        );
        this.toast(queuedFailure.title, message, true);
        return {
          unknown: false,
          inspection,
          queuedFailure: true,
          code: error.code,
          message,
          requestId,
          jobId
        };
      }
      const csrfRejected = Boolean(!unknown && !inspection && error && error.preAcceptance === true && error.csrfRejected === true);
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
      const message = inspection
        ? withCorrelation(
          `${observed} ${formatting.inspectionGuidance || "This multi-stage save is only partially applied. Do not submit it again; inspect Activity and Logs before reconciling the current profile and credential state."}`,
          formatting.inspectionFallback || "The operation was only partially applied. Do not retry it; inspect Activity and Logs."
        )
        : unknown
          ? withCorrelation(
            formatting.unknownGuidance
              ? `${observed} ${formatting.unknownGuidance}`
              : (error && error.acceptanceUnknown === true
                ? `${observed} Preserve the client request ID and inspect Activity and Logs before taking any further action.`
                : `${observed} The request was already queued; do not retry it or create a duplicate. Inspect Activity and Logs for the eventual outcome.`),
            formatting.unknownFallback || "The operation outcome is unknown. Do not retry it; inspect Activity and Logs."
          )
          : withCorrelation(observed, fallback);
      this.toast(unknown || inspection ? unknownTitle : failedTitle, message, !unknown && !inspection);
      return { unknown, inspection, message, requestId, jobId };
    },
    async reconcileProfileIncident(event) {
      if (event && event.preventDefault) event.preventDefault();
      const incident = this.profileReconciliationIncident;
      if (!incident || !this.canReconcileProfileIncident) return;
      const requestId = incident.requestId;
      const operation = incident.operation;
      this.operationBusy = true;
      this.profileReconciliationState = "checking";
      this.profileSaveMessage = `Resolving client request ${requestId} against the authenticated private queue…`;
      this.toast(
        "Profile reconciliation started",
        "Looking up the exact preserved request ID. This read-only recovery does not submit or replay profile configuration or credentials."
      );
      try {
        const recovered = await reconcileMutationRequest(
          this.auth,
          requestId,
          operation,
          undefined,
          AUTOSAVE_API_LIMITS
        );
        if (this.disposed || this.profileReconciliationIncident !== incident) return;
        if (!recovered
          || recovered.schema !== "sdsync.dsm-reconciled-result.v1"
          || recovered.request_id !== requestId
          || recovered.operation !== operation
          || !validatedJobId(recovered.job_id)
          || (validatedJobId(incident.jobId) && recovered.job_id !== incident.jobId)
          || !recovered.result
          || recovered.result.ok !== true) {
          throw new QueuedOutcomeUnknownError(
            recovered && recovered.job_id,
            "DSM returned an invalid reconciled profile result. The preserved request remains locked.",
            requestId,
            operation,
            "request_reconciliation"
          );
        }

        this.profileSaveMessage = "The request completed. Verifying current package state without replacing the open draft…";
        const refreshed = await this.refreshSnapshot(false, true);
        if (this.disposed || this.profileReconciliationIncident !== incident) return;
        const observedProfile = this.profiles.find((profile) => String(profile.name) === String(incident.subject));
        if (refreshed !== true || !observedProfile) {
          throw new QueuedOutcomeUnknownError(
            recovered.job_id,
            "The request completed, but its profile cannot yet be verified in a fresh package snapshot. The draft remains locked.",
            requestId,
            operation,
            "snapshot_reconciliation"
          );
        }
        if (incident.expectedConfiguration
          && !profileSnapshotMatchesExpected(observedProfile, incident.expectedConfiguration)) {
          throw new QueuedOutcomeUnknownError(
            recovered.job_id,
            "The request completed, but the current package profile does not exactly match the submitted non-secret configuration. The draft remains locked.",
            requestId,
            operation,
            "snapshot_reconciliation"
          );
        }

        if (!this.selectedProfile) this.selectedProfile = String(incident.subject);
        if (incident.expectedConfiguration) {
          this.hydrateAutosave("profile", incident.expectedConfiguration, false);
        }
        if (operation === ACTIONS.configureProfile) {
          this.clearProfileConfigurationFailure(false);
          this.profileSaveState = "success";
          this.profileSaveMessage = "Profile configuration reconciled. Protected credential drafts were not submitted; review and save them explicitly.";
          this.toast(
            "Profile configuration reconciled",
            "DSM confirmed the exact queued request and the fresh snapshot matches. Protected credential drafts remain untouched."
          );
        } else {
          if (!this.applyTrustedSecretPresence(recovered.result)) {
            throw new QueuedOutcomeUnknownError(
              recovered.job_id,
              "DSM completed the secret request, but returned no trustworthy secret-presence result. The draft remains locked.",
              requestId,
              operation,
              "request_reconciliation"
            );
          }
          const field = incident.secretKind === "remote-log-token" ? "remote_log_token" : incident.secretKind;
          if (this.secretModes && Object.prototype.hasOwnProperty.call(this.secretModes, field)) this.secretModes[field] = "keep";
          if (this.secretValues && Object.prototype.hasOwnProperty.call(this.secretValues, field)) this.secretValues[field] = "";
          this.clearProfileSecretFailures([incident.secretKind], false);
          this.profileSaveState = "success";
          this.profileSaveMessage = `Protected ${incident.secretKind} operation reconciled. Other credential drafts remain untouched.`;
          this.toast(
            "Protected credential reconciled",
            `DSM confirmed only the ${incident.secretKind} operation. Other credential drafts were not submitted or cleared.`
          );
        }
        this.syncProfileFailureState();
      } catch (caught) {
        if (this.disposed || this.profileReconciliationIncident !== incident) return;
        const knownTerminalFailure = Boolean(
          caught
          && caught.accepted === true
          && caught.outcomeUnknown !== true
          && caught.trustedJobId === true
          && validatedJobId(caught.jobId)
          && (!validatedJobId(incident.jobId) || caught.jobId === incident.jobId)
          && caught.trustedRequestId === true
          && caught.requestId === requestId
          && caught.operation === operation
        );
        if (knownTerminalFailure) {
          const refreshed = await this.refreshSnapshot(false, true);
          if (this.disposed || this.profileReconciliationIncident !== incident) return;
          const observedProfile = this.profiles.find((profile) => String(profile.name) === String(incident.subject));
          if (incident.expectedConfiguration
            && refreshed === true
            && observedProfile
            && profileSnapshotMatchesExpected(observedProfile, incident.expectedConfiguration)) {
            this.hydrateAutosave("profile", incident.expectedConfiguration, false);
          }
          if (operation === ACTIONS.configureProfile) this.clearProfileConfigurationFailure(false);
          else this.clearProfileSecretFailures([incident.secretKind], false);
          this.profileSaveState = "error";
          this.profileSaveMessage = boundedText(caught.message, "DSM rejected the preserved request. Correct the draft and try again.");
          this.toast(
            "Profile request reconciled as failed",
            `${this.profileSaveMessage} No new mutation was submitted; the settled stage is unlocked for correction.`,
            true
          );
          this.syncProfileFailureState();
          return;
        }
        this.profileSaveState = "error";
        const report = this.reportMutationError(
          caught,
          "Profile reconciliation failed",
          "Profile reconciliation still unresolved",
          "The exact request could not be reconciled.",
          {
            unknownGuidance: "No new mutation was submitted. Keep the draft open and try reconciliation again after checking Activity / Logs.",
            inspectionGuidance: "No new mutation was submitted. Keep the draft open and verify current profile state before another save."
          }
        );
        this.profileSaveMessage = report.message;
      } finally {
        if (!this.disposed) {
          this.profileReconciliationState = "idle";
          this.operationBusy = false;
        }
      }
    },
    async reconcileConnectionIncident(event) {
      if (event && event.preventDefault) event.preventDefault();
      const incident = this.connectionReconciliationIncident;
      if (!incident || !this.canReconcileConnectionIncident) return;
      const requestId = incident.requestId;
      const operation = incident.operation;
      this.operationBusy = true;
      this.profileReconciliationState = "checking";
      this.profileConnectionMessage = `Resolving client request ${requestId} against the authenticated private queue…`;
      this.toast(
        "Connection reconciliation started",
        "Looking up the exact preserved request ID. This read-only recovery does not start another File Station session."
      );
      try {
        const recovered = await reconcileMutationRequest(
          this.auth,
          requestId,
          operation,
          undefined,
          PROFILE_CONNECTION_API_LIMITS
        );
        if (this.disposed || this.connectionReconciliationIncident !== incident) return;
        if (!recovered
          || recovered.schema !== "sdsync.dsm-reconciled-result.v1"
          || recovered.request_id !== requestId
          || recovered.operation !== operation
          || !validatedJobId(recovered.job_id)
          || (validatedJobId(incident.jobId) && recovered.job_id !== incident.jobId)
          || !recovered.result
          || recovered.result.ok !== true) {
          throw new QueuedOutcomeUnknownError(
            recovered && recovered.job_id,
            "DSM returned an invalid reconciled connection result. The preserved request remains locked.",
            requestId,
            operation,
            "request_reconciliation"
          );
        }
        if (operation === ACTIONS.testProfileAuth) {
          const proof = boundedText(recovered.result.connection_proof, "");
          const expires = Number(recovered.result.connection_proof_expires_at_epoch);
          const proofExpires = Number(proof.split(".")[1]);
          if (!/^v1\.[0-9]+\.[0-9a-f]{64}\.[0-9a-f]{64}$/.test(proof)
            || !Number.isSafeInteger(expires)
            || !Number.isSafeInteger(proofExpires)
            || proofExpires !== expires) {
            throw new QueuedOutcomeUnknownError(
              recovered.job_id,
              "The reconciled authentication result is invalid. The preserved request remains locked.",
              requestId,
              operation,
              "request_reconciliation"
            );
          }
          this.connectionProof = "";
          this.connectionProofExpires = 0;
          this.clearConnectionProofTimer();
          this.profileConnectionState = "idle";
          this.profileConnectionMessage = "The previous authentication request succeeded and settled safely. Test the current draft once more to unlock File Station browsing.";
        } else {
          this.profileConnectionState = "idle";
          this.profileConnectionMessage = "The previous File Station browse request settled. You may browse again.";
          if (this.pathBrowser && this.pathBrowser.visible && this.pathBrowser.kind === "remote") this.pathBrowser.error = "";
        }
        clearIsolatedIncident(this, "connection");
        this.toast(
          operation === ACTIONS.testProfileAuth ? "Authentication reconciled" : "File Station request reconciled",
          operation === ACTIONS.testProfileAuth
            ? "DSM confirmed the exact queued authentication request and its temporary session cleanup. Test the current draft again before browsing."
            : "DSM confirmed the exact queued browse request. No new File Station request was submitted."
        );
      } catch (caught) {
        if (this.disposed || this.connectionReconciliationIncident !== incident) return;
        const knownTerminalFailure = Boolean(
          caught
          && caught.accepted === true
          && caught.outcomeUnknown !== true
          && caught.trustedJobId === true
          && validatedJobId(caught.jobId)
          && (!validatedJobId(incident.jobId) || caught.jobId === incident.jobId)
          && caught.trustedRequestId === true
          && caught.requestId === requestId
          && caught.operation === operation
        );
        if (knownTerminalFailure) {
          clearIsolatedIncident(this, "connection");
          this.profileConnectionState = "error";
          this.profileConnectionMessage = boundedText(caught.message, "DSM rejected the preserved connection request. Correct the draft and try again.");
          this.toast(
            "Connection request reconciled as failed",
            `${this.profileConnectionMessage} No new request was submitted; another authentication or File Station request is now permitted.`,
            true
          );
          return;
        }
        this.profileConnectionState = "error";
        const report = this.reportMutationError(
          caught,
          "Connection reconciliation failed",
          "Connection reconciliation still unresolved",
          "The exact request could not be reconciled.",
          {
            unknownGuidance: "No new request was submitted. Keep the draft open and try reconciliation again after checking Activity / Logs.",
            inspectionGuidance: "No new request was submitted. Keep the draft open until the exact request can be reconciled."
          }
        );
        this.profileConnectionMessage = report.message;
      } finally {
        if (!this.disposed) {
          this.profileReconciliationState = "idle";
          this.operationBusy = false;
        }
      }
    },
    hasCapability(name) { return this.capabilities[name] === true; },
    // Keep looking, on a decaying interval, instead of stopping at the first
    // recovery window. One bounded read pass per locked incident per tick, so
    // the cost does not grow with the number of locked scopes.
    scheduleIncidentProbe() {
      window.clearTimeout(this.incidentProbeTimer);
      this.incidentProbeTimer = 0;
      if (this.disposed || document.hidden || !probeableIncidents(this).length) return;
      const step = Math.min(this.incidentProbeStep, INCIDENT_PROBE_RAMP_MS.length - 1);
      this.incidentProbeTimer = window.setTimeout(() => this.checkIncidentOutcomes(), INCIDENT_PROBE_RAMP_MS[step]);
    },
    // A scope whose lock rested only on not knowing. The package has now proved
    // what became of the exact request, so the premise is gone with it.
    releaseIncidentScope(scope) {
      if (scope === "operations") return clearIsolatedIncident(this, scope);
      if (this.autosaveFailureScopes) this.autosaveFailureScopes[scope] = false;
      if (this.autosaveOutcomeUnknownScopes) this.autosaveOutcomeUnknownScopes[scope] = false;
      if (this.autosaveInspectionScopes) this.autosaveInspectionScopes[scope] = false;
      clearScopeIncident(this, scope);
      this.refreshAutosaveStatus();
    },
    // Ask the package what became of every locked request. Applies nothing, and
    // releases a lock only where the verdict disproves the premise it rests on:
    // this replaces "we stopped looking" with what is actually knowable now.
    async checkIncidentOutcomes(manual = false) {
      if (this.disposed || this.incidentProbe.active) return;
      const targets = probeableIncidents(this);
      if (!targets.length) return this.scheduleIncidentProbe();
      if (manual) this.incidentProbeStep = 0;
      this.incidentProbe = { ...this.incidentProbe, active: true };
      const released = [];
      try {
        for (const { scope, incident } of targets) {
          let observed = null;
          try {
            observed = await probeRequestOutcome(this.auth, incident.requestId, incident.operation, AUTOSAVE_API_LIMITS);
          } catch (_error) {
            observed = null;
          }
          if (this.disposed) return;
          const verdict = observed && PROBE_VERDICT_COPY[observed.verdict] ? observed.verdict : "unavailable";
          this.incidentProbe = {
            active: true,
            scope,
            verdict,
            attempts: this.incidentProbe.attempts + 1,
            checkedAt: (observed && observed.checked_at) || Date.now(),
            jobId: (observed && observed.job_id) || "",
            progress: (observed && observed.progress) || null,
            message: PROBE_VERDICT_COPY[verdict]
          };
          if (verdict === "settled" && SELF_RELEASING_INCIDENT_SCOPES.includes(scope)) released.push(scope);
        }
      } finally {
        if (!this.disposed) this.incidentProbe = { ...this.incidentProbe, active: false };
      }
      if (this.disposed) return;
      this.incidentProbeStep += 1;
      if (released.length) {
        released.forEach((scope) => this.releaseIncidentScope(scope));
        this.toast(
          "Outcome established",
          guidanceText(
            `DSM holds a completed record for the preserved request, so ${released.map((scope) => INCIDENT_SCOPE_LABELS[scope]).join(" and ")} ${released.length === 1 ? "is" : "are"} unlocked.`,
            "No new request was submitted. Review the refreshed state before repeating the change."
          )
        );
        await this.refreshSnapshot(false, true);
        if (this.disposed) return;
      }
      this.scheduleIncidentProbe();
    },
    integer(value, fallback) { const parsed = Number(value); return Number.isInteger(parsed) ? parsed : fallback; },
    between(value, minimum, maximum) { const parsed = Number(value); return Number.isInteger(parsed) && parsed >= minimum && parsed <= maximum; },
    toast(title, message, error = false) { if (this.disposed) return; const item = { id: ++this.toastSequence, title, message, error }; this.toasts.push(item); const timer = window.setTimeout(() => { if (this.disposed) return; const index = this.toasts.findIndex((candidate) => candidate.id === item.id); if (index >= 0) this.toasts.splice(index, 1); this.toastTimers = this.toastTimers.filter((candidate) => candidate !== timer); }, 6000); this.toastTimers.push(timer); },
    installProfileWindowGuard() {
      if (this.beforeUnloadHandler) return;
      this.beforeUnloadHandler = (event) => {
        if (!this.profileWindowGuardActive) return undefined;
        event.preventDefault();
        event.returnValue = "";
        return "";
      };
      window.addEventListener("beforeunload", this.beforeUnloadHandler);
    },
    removeProfileWindowGuard() {
      if (!this.beforeUnloadHandler) return;
      window.removeEventListener("beforeunload", this.beforeUnloadHandler);
      this.beforeUnloadHandler = null;
    },
    setProfileCreationStage(current, total, message) {
      const boundedTotal = Math.max(1, Number(total) || 1);
      const boundedCurrent = Math.min(boundedTotal, Math.max(1, Number(current) || 1));
      const stageMessage = boundedText(message, "Creating the profile…");
      this.profileCreationProgress = { active: true, current: boundedCurrent, total: boundedTotal, message: stageMessage };
      this.profileSaveMessage = `Step ${boundedCurrent} of ${boundedTotal}: ${stageMessage} ${PROFILE_CREATION_WINDOW_WARNING}`;
    },
    clearProfileCreationProgress() { this.profileCreationProgress = emptyProfileCreationProgress(); },
    stopTimers() { window.clearTimeout(this.snapshotTimer); window.clearTimeout(this.logTimer); window.clearTimeout(this.incidentProbeTimer); this.snapshotTimer = 0; this.logTimer = 0; this.incidentProbeTimer = 0; },
    scheduleSnapshot() { window.clearTimeout(this.snapshotTimer); this.snapshotTimer = 0; const interval = Number(this.settings.status_refresh); if (interval > 0 && !this.disposed && !document.hidden && !this.snapshotRefreshBlocked) this.snapshotTimer = window.setTimeout(() => this.refreshSnapshot(false), interval); },
    scheduleLogs() { window.clearTimeout(this.logTimer); this.logTimer = 0; const interval = Number(this.settings.log_refresh); if (interval > 0 && !this.disposed && !document.hidden && this.route === "activity" && !this.logsPaused) this.logTimer = window.setTimeout(() => this.refreshLogs(), interval); },
    async refreshCsrf(options = undefined) { if (this.disposed) return; this.csrfToken = ""; const model = await apiGet(this.auth, "csrf", {}, options); if (this.disposed) return; if (typeof model.csrf_token !== "string" || !model.csrf_token || model.csrf_token.length > 4096) throw new Error("Authenticated bridge did not issue a valid CSRF token"); this.csrfToken = model.csrf_token; },
    async refreshSnapshot(manual, requirePostMutationRead = false) {
      if (this.disposed || document.hidden) return false;
      if (this.snapshotRefreshBlocked) {
        if (manual) this.toast("Status refresh paused", "Close the profile editor to refresh package status. The active profile and secret draft will not be overwritten.");
        return false;
      }
      if (this.snapshotPromise) {
        if (requirePostMutationRead) this.snapshotRefreshQueued = true;
        return this.snapshotPromise;
      }
      this.snapshotLoading = true;
      const generation = this.snapshotGeneration;
      let cycle;
      cycle = (async () => {
        let succeeded = false;
        try {
          if (!this.csrfToken) await this.refreshCsrf();
          if (this.disposed || generation !== this.snapshotGeneration || this.snapshotRefreshBlocked) return false;
          const snapshot = await apiGet(this.auth, "snapshot");
          if (this.disposed || generation !== this.snapshotGeneration || this.snapshotRefreshBlocked) return false;
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
          succeeded = true;
          if (manual) this.toast("Status refreshed", "The latest package snapshot is displayed.");
        } catch (error) {
          if (this.disposed || generation !== this.snapshotGeneration || this.snapshotRefreshBlocked) return false;
          this.csrfToken = "";
          this.connected = false;
          this.bridgeIssue = this.describeBridgeError(error, "status");
          this.connectionLabel = this.bridgeIssue.title;
          this.freshness = this.snapshot ? "Stale · last successful snapshot retained" : "Status unavailable";
          if (manual) this.toast(this.bridgeIssue.title, this.bridgeIssue.message, true);
        } finally {
          if (this.snapshotPromise === cycle) this.snapshotPromise = null;
          this.snapshotLoading = false;
          const followUp = this.snapshotRefreshQueued;
          this.snapshotRefreshQueued = false;
          if (followUp) {
            if (!this.disposed && !document.hidden && !this.snapshotRefreshBlocked) return this.refreshSnapshot(false, false);
            return false;
          }
          if (!this.disposed) this.scheduleSnapshot();
        }
        return succeeded;
      })();
      this.snapshotPromise = cycle;
      return cycle;
    },
    hydrateAlerts() {
      const alerts = this.snapshot && this.snapshot.alerts;
      if (!alerts || typeof alerts !== "object") return;
      const state = this.autosaveCoordinator ? this.autosaveCoordinator.getState("alerts") : null;
      if (this.alertDirty || (state && (state.dirty || state.inFlight))) return;
      this.alertForm = { enabled: alerts.enabled === true, on_success: alerts.on_success === true, on_failure: alerts.on_failure !== false, failure_threshold: numberOr(alerts.failure_threshold, 1), cooldown_seconds: numberOr(alerts.cooldown_seconds, 3600) };
      this.hydrateAutosave("alerts", this.alertPayload());
    },
    hydrateSecurityPolicy(force = false) {
      const state = this.autosaveCoordinator ? this.autosaveCoordinator.getState("security") : null;
      const recovering = !force
        && Boolean(this.autosaveFailureScopes && this.autosaveFailureScopes.security === true)
        && this.securityDirty === false
        && !(state && state.inFlight);
      if (!force && !recovering && (this.securityDirty || (state && (state.dirty || state.inFlight)))) return;
      const policy = normalizedSecurityPolicy(this.snapshot && this.snapshot.security_policy);
      this.securityForm = Object.assign({}, policy, { log_levels: Object.assign({}, policy.log_levels) });
      this.securityDirty = false;
      this.hydrateAutosave("security", this.securityPayload());
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
      if (scopeMutationOutcomeUnresolved(this, "security")) return this.toast("Security policy save locked", scopeMutationGuidance(this, "security"), true);
      if (!this.canMutate || !this.securityDirty || this.operationBusy) return;
      this.cancelAutosave("security");
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
          this.hydrateAutosave("security", payload);
          const appliedPolicy = new QueuedOutcomeUnknownError(
            "",
            "DSM applied the security policy, but refreshed request authentication is unavailable. Do not save the policy again; inspect Activity and Logs."
          );
          this.pauseAutosave("security", appliedPolicy);
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
        await this.refreshSnapshot(false, true);
        if (!this.disposed) this.hydrateSecurityPolicy(true);
      } catch (caught) {
        if (this.disposed) return;
        this.pauseAutosave("security", caught);
        const report = this.reportMutationError(caught, "Security policy not saved", "Security policy outcome unknown", "The package rejected the security policy.");
        if (report.unknown) {
          this.csrfToken = "";
          this.securityDirty = false;
          await this.refreshSnapshot(false, true);
          if (!this.disposed && this.connected) this.hydrateSecurityPolicy(true);
        }
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    clearProfileFilters() { this.profileFilter = ""; this.profileFilterStatus = "all"; },
    clearConnectionProofTimer() {
      if (this.connectionProofTimer && typeof window !== "undefined") window.clearTimeout(this.connectionProofTimer);
      this.connectionProofTimer = 0;
    },
    scheduleConnectionProofExpiry(expires) {
      this.clearConnectionProofTimer();
      if (typeof window === "undefined" || !window.setTimeout) return;
      const expire = () => {
        this.connectionProofTimer = 0;
        if (this.disposed || !this.connectionProof || this.connectionProofExpires !== expires) return;
        const remaining = (expires * 1000) - Date.now();
        if (remaining > 0) {
          this.connectionProofTimer = window.setTimeout(expire, Math.min(remaining + 25, 2147483647));
          return;
        }
        this.connectionProof = "";
        this.connectionProofExpires = 0;
        this.profileConnectionState = "expired";
        this.profileConnectionMessage = "Authentication proof expired. Test this unchanged draft again to browse File Station.";
        if (this.pathBrowser.visible && this.pathBrowser.kind === "remote") this.closePathBrowser();
      };
      this.connectionProofTimer = window.setTimeout(expire, Math.max(0, Math.min((expires * 1000) - Date.now() + 25, 2147483647)));
    },
    invalidateConnectionTest() {
      this.profileConnectionRequest += 1;
      this.clearConnectionProofTimer();
      this.connectionProof = "";
      this.connectionProofExpires = 0;
      if (this.profileEditorOpen) {
        this.profileConnectionState = "idle";
        this.profileConnectionMessage = "Connection or credential draft changed. Test authentication again to browse File Station.";
      }
      if (this.pathBrowser.visible && this.pathBrowser.kind === "remote") this.closePathBrowser();
    },
    connectionRequestPayload() {
      const existing = this.selectedProfileModel;
      let passwordSource = "none";
      let password = null;
      if (this.secretModes.password === "replace") {
        passwordSource = "provided";
        password = this.secretValues.password;
        if (!password) return { error: "Enter the DSM password before testing authentication." };
      } else if (this.secretModes.password === "keep" && this.selectedProfile && existing && existing.has_password === true) {
        passwordSource = "stored";
      } else {
        return { error: this.selectedProfile ? "This profile has no stored password. Choose Replace securely and enter one." : "A new profile needs a password. Enter it under Protected credentials." };
      }
      let totpSource = "none";
      let totp = null;
      if (this.secretModes.totp === "replace") {
        totpSource = "provided";
        totp = this.secretValues.totp;
        if (!totp) return { error: "Enter the Base32 TOTP seed or otpauth URI, or choose Clear/Keep when TOTP is not required." };
      } else if (this.secretModes.totp === "keep" && this.selectedProfile && existing && existing.has_totp === true) {
        totpSource = "stored";
      }
      const connectTimeout = this.strictDraftInteger(this.profileForm.connect_timeout);
      const timeout = this.strictDraftInteger(this.profileForm.timeout);
      const retries = this.strictDraftInteger(this.profileForm.retries);
      if (connectTimeout === null || timeout === null || retries === null) return { error: "Finish the connection timeout, upload timeout, and retries with whole numbers." };
      if (!validBoundedText(this.profileForm.url, 2048)
        || !(this.profileForm.url.startsWith("https://") || (this.profileForm.allow_http && this.profileForm.url.startsWith("http://")))) return { error: "Enter a valid HTTPS File Station URL, or explicitly allow HTTP for a controlled LAN target." };
      if (!validBoundedText(this.profileForm.username, 256)) return { error: "Enter a valid DSM username." };
      if (!this.between(connectTimeout, 1, 600) || !this.between(timeout, 1, 86400) || !this.between(retries, 0, 5)) return { error: "Connection timeout, upload timeout, or retries is outside the supported range." };
      if (this.profileForm.ca_certificate && (!validBoundedText(this.profileForm.ca_certificate, 4096) || !this.profileForm.ca_certificate.startsWith("/") || hasDotPathSegment(this.profileForm.ca_certificate))) return { error: "CA certificate must be an absolute NAS path without dot segments." };
      return {
        profile: this.selectedProfile || null,
        url: this.profileForm.url,
        username: this.profileForm.username,
        allow_http: this.profileForm.allow_http === true,
        danger_accept_invalid_certs: this.profileForm.danger_invalid_certs === true,
        ca_certificate: this.profileForm.ca_certificate || null,
        connect_timeout_seconds: connectTimeout,
        timeout_seconds: timeout,
        retries,
        password_source: passwordSource,
        password,
        totp_source: totpSource,
        totp
      };
    },
    async testProfileAuthentication(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (scopeMutationOutcomeUnresolved(this, "profile")) return this.toast("Authentication test locked", scopeMutationGuidance(this, "profile"), true);
      if (isolatedIncidentUnresolved(this, "connection")) return this.toast("Authentication test locked", isolatedIncidentGuidance(this, "connection"), true);
      if (!this.canTestProfileAuthentication) return this.toast("Authentication test unavailable", "Wait for the current profile operation to finish and confirm that operational actions are permitted.", true);
      const payload = this.connectionRequestPayload();
      if (payload.error) return this.toast("Authentication not tested", payload.error, true);
      const priorEvidence = this.connectionIncidentEvidence || "";
      this.holdProfileAutosaveForConnection();
      const request = ++this.profileConnectionRequest;
      this.operationBusy = true;
      this.connectionProof = "";
      this.connectionProofExpires = 0;
      this.profileConnectionState = "testing";
      this.profileConnectionMessage = priorEvidence
        ? `Testing the current draft again. ${PROFILE_CONNECTION_HEALTHY_TIMING} Prior unresolved evidence remains available for explicit reconciliation: ${priorEvidence}`
        : `Discovering File Station and testing this draft. ${PROFILE_CONNECTION_HEALTHY_TIMING}`;
      this.toast(
        "Authentication test started",
        priorEvidence
          ? `${PROFILE_CONNECTION_HEALTHY_TIMING} Prior unresolved evidence remains preserved for explicit reconciliation.`
          : `${PROFILE_CONNECTION_HEALTHY_TIMING} Success is reported only after temporary session cleanup finishes.`
      );
      try {
        const result = await apiPost(this.auth, this.csrfToken, ACTIONS.testProfileAuth, payload, true, undefined, PROFILE_CONNECTION_API_LIMITS);
        if (this.disposed || request !== this.profileConnectionRequest || !this.profileEditorOpen) return;
        const proof = boundedText(result.connection_proof, "");
        const expires = Number(result.connection_proof_expires_at_epoch);
        const proofExpires = Number(proof.split(".")[1]);
        if (!/^v1\.[0-9]+\.[0-9a-f]{64}\.[0-9a-f]{64}$/.test(proof) || !Number.isSafeInteger(expires) || !Number.isSafeInteger(proofExpires) || proofExpires !== expires || expires <= Math.floor(Date.now() / 1000)) throw new Error("The package returned an invalid authentication proof.");
        this.connectionProof = proof;
        this.connectionProofExpires = expires;
        this.scheduleConnectionProofExpiry(expires);
        this.profileConnectionState = "success";
        this.profileConnectionMessage = isolatedIncidentUnresolved(this, "connection")
          ? `Authentication succeeded, but prior unresolved connection evidence remains: ${isolatedIncidentGuidance(this, "connection")}`
          : "Authentication succeeded. File Station browsing is unlocked for this unchanged draft.";
        if (isolatedIncidentUnresolved(this, "connection")) {
          this.toast("Authentication succeeded · prior evidence remains", isolatedIncidentGuidance(this, "connection"), true);
        } else {
          this.toast("Authentication succeeded", "The temporary File Station session was closed; no draft credential was stored by the test.");
        }
      } catch (error) {
        if (this.disposed || request !== this.profileConnectionRequest || !this.profileEditorOpen) return;
        this.profileConnectionState = "error";
        const uncertain = Boolean(error && (error.outcomeUnknown === true || error.requiresInspection === true));
        const retryGuidance = "Keep this evidence while inspecting Activity / Logs. Resolve the exact client request before changing this preserved draft or starting another authentication or File Station request; unrelated autosave remains available.";
        const report = this.reportMutationError(
          error,
          "Authentication failed",
          error && error.outcomeUnknown === true ? "Authentication outcome unknown" : "Authentication cleanup needs inspection",
          "Authentication failed.",
          uncertain ? { inspectionGuidance: retryGuidance, unknownGuidance: retryGuidance } : undefined
        );
        recordIsolatedIncident(this, "connection", "Authentication test", error, report, {
          subject: `${this.selectedProfile || "New profile"} · ${this.profileForm.url}`,
          operation: ACTIONS.testProfileAuth
        });
        this.profileConnectionMessage = report.message;
      } finally {
        if (!this.disposed) {
          this.operationBusy = false;
          this.releaseProfileAutosaveFromConnection();
        }
      }
    },
    openLocalSourceBrowser(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (!this.profileEditorOpen || !this.canChangeProfiles) return;
      const initial = validLocalSourcePath(this.profileForm.source) ? this.profileForm.source : "/";
      return this.showPathBrowser("local", initial);
    },
    openRemotePathBrowser(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (scopeMutationOutcomeUnresolved(this, "profile")) return this.toast("File Station browse locked", scopeMutationGuidance(this, "profile"), true);
      if (isolatedIncidentUnresolved(this, "connection")) return this.toast("File Station browse locked", isolatedIncidentGuidance(this, "connection"), true);
      if (this.connectionProofExpires <= Math.floor(Date.now() / 1000)) this.invalidateConnectionTest();
      if (!this.connectionTestReady) return this.toast("Test authentication first", "The File Station browser unlocks only after this exact connection and credential draft authenticates successfully.", true);
      const initial = this.profileForm.remote && this.profileForm.remote.startsWith("/") ? this.profileForm.remote : "/";
      return this.showPathBrowser("remote", initial);
    },
    showPathBrowser(kind, initial) {
      const requestGeneration = (this.pathBrowser && Number(this.pathBrowser.request)) || 0;
      this.removePathBrowserKeyHandler();
      this.pathBrowserPriorFocus = typeof document !== "undefined" ? document.activeElement : null;
      this.pathBrowserKeyHandler = (event) => this.handlePathBrowserKeydown(event);
      if (typeof document !== "undefined") document.addEventListener("keydown", this.pathBrowserKeyHandler, true);
      this.pathBrowser = Object.assign(emptyPathBrowser(), { visible: true, kind, current: initial, parent: this.browserParent(initial), request: requestGeneration });
      if (typeof this.$nextTick === "function") {
        this.$nextTick(() => {
          if (!this.pathBrowser.visible || this.disposed) return;
          const initialFocus = this.confirmationElement("pathBrowserClose") || this.confirmationElement("pathBrowserDialog");
          if (initialFocus && initialFocus.focus) initialFocus.focus();
        });
      }
      return this.browsePath(initial);
    },
    browserParent(path) {
      if (!path || path === "/") return null;
      const index = path.lastIndexOf("/");
      return index <= 0 ? "/" : path.slice(0, index);
    },
    pathBrowserBreadcrumbs(path) {
      const supplied = typeof path === "string" && path.startsWith("/") ? path : "/";
      const current = supplied.length > 1 ? supplied.replace(/\/+$/, "") : "/";
      const parts = current.split("/").filter(Boolean);
      const breadcrumbs = [{
        label: this.pathBrowser.kind === "remote" ? "File Station" : "NAS",
        path: "/",
        current: parts.length === 0
      }];
      let cursor = "";
      for (const part of parts) {
        cursor += `/${part}`;
        breadcrumbs.push({ label: part, path: cursor, current: cursor === current });
      }
      return breadcrumbs;
    },
    async browsePath(path) {
      if (!this.pathBrowser.visible || this.pathBrowser.loading || !path) return;
      const kind = this.pathBrowser.kind;
      if (kind === "remote" && scopeMutationOutcomeUnresolved(this, "profile")) {
        this.pathBrowser.error = scopeMutationGuidance(this, "profile");
        return;
      }
      if (kind === "remote" && this.connectionProofExpires <= Math.floor(Date.now() / 1000)) {
        this.invalidateConnectionTest();
        return this.toast("Authentication expired", "Test authentication again before browsing File Station.", true);
      }
      if (kind === "remote" && isolatedIncidentUnresolved(this, "connection")) {
        this.pathBrowser.error = isolatedIncidentGuidance(this, "connection");
        return;
      }
      if (kind === "remote" && !this.connectionTestReady) {
        this.closePathBrowser();
        return this.toast("Authentication expired", "Test authentication again before browsing File Station.", true);
      }
      const request = this.pathBrowser.request + 1;
      let remoteConnection = null;
      let remotePayload = null;
      if (kind === "remote") {
        remoteConnection = this.connectionRequestPayload();
        if (remoteConnection.error) return this.toast("File Station browse unavailable", remoteConnection.error, true);
        remotePayload = Object.assign({}, remoteConnection, { parent: path, connection_proof: this.connectionProof });
      }
      const priorEvidence = kind === "remote" ? (this.connectionIncidentEvidence || "") : "";
      const remoteIncidentSubject = kind === "remote"
        ? `${this.selectedProfile || "New profile"} · ${path}`
        : "";
      if (kind === "remote") {
        this.holdProfileAutosaveForConnection();
        this.operationBusy = true;
      }
      this.pathBrowser.request = request;
      this.pathBrowser.current = path;
      this.pathBrowser.parent = this.browserParent(path);
      this.pathBrowser.directories = [];
      this.pathBrowser.truncated = false;
      this.pathBrowser.loading = true;
      this.pathBrowser.error = priorEvidence
        ? `Issuing a new browse request while prior unresolved evidence remains visible for explicit reconciliation: ${priorEvidence}`
        : "";
      try {
        let result;
        if (kind === "local") {
          result = await apiGet(this.auth, "source-directories", { parent: path });
          if (result.schema !== "sdsync.dsm-source-directories.v1") throw new Error("The package returned an unsupported source-browser document.");
        } else {
          result = await apiPost(this.auth, this.csrfToken, ACTIONS.browseRemote, remotePayload, true, undefined, PROFILE_CONNECTION_API_LIMITS);
          if (result.directory_schema !== "sdsync.dsm-remote-directories.v1") throw new Error("The package returned an unsupported File Station browser document.");
        }
        if (this.disposed || !this.pathBrowser.visible || request !== this.pathBrowser.request || kind !== this.pathBrowser.kind) return;
        const current = boundedText(result.current, "");
        const directories = arrayOf(result.directories).filter((entry) => entry && typeof entry === "object" && validBoundedText(entry.name, 255) && validBoundedText(entry.path, kind === "local" ? 4096 : 247)).map((entry) => ({ name: entry.name, path: entry.path }));
        if (current !== path || directories.length !== arrayOf(result.directories).length) throw new Error("The package returned an invalid directory listing.");
        this.pathBrowser.current = current;
        this.pathBrowser.parent = kind === "local" ? (result.parent === null ? null : boundedText(result.parent, "")) : this.browserParent(current);
        this.pathBrowser.directories = directories;
        this.pathBrowser.truncated = result.truncated === true;
        if (kind === "remote") {
          this.pathBrowser.error = isolatedIncidentUnresolved(this, "connection")
            ? `Directory listing succeeded, but prior unresolved connection evidence remains: ${isolatedIncidentGuidance(this, "connection")}`
            : "";
        }
      } catch (error) {
        if (this.disposed) return;
        const presentationCurrent = this.pathBrowser.visible
          && request === this.pathBrowser.request
          && kind === this.pathBrowser.kind;
        const detail = boundedText(error && error.message, "Directory listing failed.");
        if (kind === "remote") {
          const uncertain = Boolean(error && (error.outcomeUnknown === true || error.requiresInspection === true));
          if (!presentationCurrent && !uncertain) return;
          const retryGuidance = "Keep this evidence while inspecting Activity / Logs. Resolve the exact client request before changing this preserved draft or starting another authentication or File Station request; unrelated autosave remains available.";
          const report = this.reportMutationError(
            error,
            "File Station browse failed",
            error && error.outcomeUnknown === true ? "File Station browse outcome unknown" : "File Station browse cleanup needs inspection",
            detail,
            uncertain ? { inspectionGuidance: retryGuidance, unknownGuidance: retryGuidance } : undefined
          );
          recordIsolatedIncident(this, "connection", "File Station browse", error, report, {
            subject: remoteIncidentSubject,
            operation: ACTIONS.browseRemote
          });
          if (presentationCurrent) this.pathBrowser.error = report.message;
        } else {
          if (!presentationCurrent) return;
          this.pathBrowser.error = `${detail} Confirm the folder exists, is a canonical non-symlink DSM volume path, is not DSM-managed, and the package identity can read and traverse it.`;
        }
      } finally {
        if (!this.disposed && this.pathBrowser.visible && request === this.pathBrowser.request) this.pathBrowser.loading = false;
        if (!this.disposed && kind === "remote") {
          this.operationBusy = false;
          this.releaseProfileAutosaveFromConnection();
        }
      }
    },
    selectPath(path) {
      if (!this.pathBrowser.visible || this.pathBrowser.loading || !path || path === "/") return;
      if (this.pathBrowser.kind === "local") this.profileForm.source = path;
      else this.profileForm.remote = path;
      this.closePathBrowser();
    },
    closePathBrowser() {
      const priorFocus = this.pathBrowserPriorFocus;
      this.removePathBrowserKeyHandler();
      this.pathBrowserPriorFocus = null;
      const request = (this.pathBrowser && Number(this.pathBrowser.request)) || 0;
      this.pathBrowser = Object.assign(emptyPathBrowser(), { request: request + 1 });
      if (!this.disposed && typeof this.$nextTick === "function") {
        this.$nextTick(() => {
          if (priorFocus && priorFocus.isConnected && priorFocus.focus) priorFocus.focus();
        });
      }
    },
    openProfile(name) {
      if (this.operationBusy) return;
      if (!name && !this.canChangeProfiles) return;
      this.snapshotGeneration += 1;
      if (typeof window !== "undefined" && typeof window.clearTimeout === "function") window.clearTimeout(this.snapshotTimer);
      this.snapshotTimer = 0;
      const profile = name ? this.profiles.find((item) => String(item.name) === String(name)) : null;
      this.selectedProfile = profile ? String(profile.name) : "";
      this.profileForm = emptyProfile();
      if (profile) this.profileForm = Object.assign(this.profileForm, {
        name: pick(profile, "name") || "", source: pick(profile, "source") || "",
        url: pick(profile, "url") || "", username: pick(profile, "username") || "",
        remote: pick(profile, "remote", "remote_path") || "",
        compare: pick(profile, "compare") || "content", jobs: numberOr(pick(profile, "jobs"), 2),
        allow_http: pick(profile, "allow_http") === true, delete: pick(profile, "delete") === true,
        max_delete: numberOr(pick(profile, "max_delete"), 100),
        make_default: pick(profile, "is_default", "default") === true,
        excludes: arrayOf(profile.excludes).join("\n"),
        allow_empty_source: pick(profile, "allow_empty_source") === true,
        retries: numberOr(pick(profile, "retries"), 2),
        timeout: numberOr(pick(profile, "timeout", "upload_timeout_seconds"), 7200),
        connect_timeout: numberOr(pick(profile, "connect_timeout", "connect_timeout_seconds"), 15),
        max_rate: numberOr(pick(profile, "max_rate", "max_rate_bytes_per_second"), 0),
        ca_certificate: pick(profile, "ca_certificate") || "",
        danger_invalid_certs: pick(profile, "danger_invalid_certs", "danger_accept_invalid_certs") === true,
        verbosity: numberOr(pick(profile, "verbosity"), 0), quiet: pick(profile, "quiet") === true,
        log_level: pick(profile, "log_level") || "info",
        log_format: pick(profile, "log_format") || "json",
        progress: pick(profile, "progress") || "never",
        output: pick(profile, "output") || "human",
        remote_log_url: pick(profile, "remote_log_url") || "",
        remote_log_mode: pick(profile, "remote_log_mode") || "best-effort"
      });
      this.clearSecrets();
      this.secretModes = profile
        ? { password: "keep", totp: "keep", remote_log_token: "keep" }
        : { password: "replace", totp: "keep", remote_log_token: "keep" };
      this.profileConnectionRequest += 1;
      this.profileConnectionState = "idle";
      this.profileConnectionMessage = profile
        ? "Test the stored or replacement credentials to unlock File Station browsing."
        : "Enter the new profile password, then test authentication to unlock File Station browsing.";
      this.connectionProof = "";
      this.connectionProofExpires = 0;
      this.clearConnectionProofTimer();
      this.profileSaveState = "idle";
      this.profileSaveMessage = "";
      this.profileCreationProgress = emptyProfileCreationProgress();
      this.profileReconciliationState = "idle";
      this.closePathBrowser();
      this.profileEditorOpen = true;
      this.freshness = "Profile draft active · status refresh paused";
      this.hydrateAutosave("profile", this.profilePayload(), false);
      if (this.autosaveCoordinator) this.autosaveCoordinator.setScopeBlocked("profile", !profile);
    },
    closeProfile(options = undefined) {
      if (this.profileSaveState === "saving"
        || this.profileConnectionState === "testing"
        || this.profileReconciliationState === "checking"
        || this.profileOutcomeUnresolved
        || this.connectionOutcomeUnresolved) return;
      this.cancelAutosave("profile");
      this.snapshotGeneration += 1;
      this.profileConnectionRequest += 1;
      this.closePathBrowser();
      this.clearSecrets();
      this.secretModes = { password: "keep", totp: "keep", remote_log_token: "keep" };
      this.profileConnectionState = "idle";
      this.profileConnectionMessage = "Test authentication to unlock the File Station browser.";
      this.connectionProof = "";
      this.connectionProofExpires = 0;
      this.clearConnectionProofTimer();
      this.profileSaveState = "idle";
      this.profileSaveMessage = "";
      this.profileCreationProgress = emptyProfileCreationProgress();
      this.profileReconciliationState = "idle";
      this.profileEditorOpen = false;
      this.selectedProfile = "";
      if (options && options.refresh === false) return;
      if (typeof this.refreshSnapshot === "function") return this.refreshSnapshot(false, true);
      if (typeof this.scheduleSnapshot === "function") this.scheduleSnapshot();
    },
    clearSecrets() { this.secretValues = { password: "", totp: "", remote_log_token: "" }; },
    applyTrustedSecretPresence(result) {
      const profile = this.selectedProfileModel;
      const fields = ["has_password", "has_totp", "has_remote_log_token"];
      if (!profile || !result || fields.some((field) => typeof result[field] !== "boolean")) return false;
      for (const field of fields) {
        if (typeof this.$set === "function") this.$set(profile, field, result[field]);
        else profile[field] = result[field];
      }
      return true;
    },
    profilePayload() {
      const maxRate = this.integer(this.profileForm.max_rate, 0);
      return {
        name: this.profileForm.name, source: this.profileForm.source, url: this.profileForm.url,
        username: this.profileForm.username, remote: this.profileForm.remote,
        compare: this.profileForm.compare, jobs: this.integer(this.profileForm.jobs, 2),
        allow_http: this.profileForm.allow_http === true, delete: this.profileForm.delete === true,
        max_delete: this.integer(this.profileForm.max_delete, 100),
        make_default: this.profileForm.make_default === true,
        excludes: String(this.profileForm.excludes || "").split(/\r?\n/).map((item) => item.trim()).filter(Boolean),
        allow_empty_source: this.profileForm.allow_empty_source === true,
        retries: this.integer(this.profileForm.retries, 2),
        timeout_seconds: this.integer(this.profileForm.timeout, 7200),
        connect_timeout_seconds: this.integer(this.profileForm.connect_timeout, 15),
        max_rate_bytes_per_second: maxRate === 0 ? null : maxRate,
        ca_certificate: this.profileForm.ca_certificate || null,
        danger_accept_invalid_certs: this.profileForm.danger_invalid_certs === true,
        verbosity: this.integer(this.profileForm.verbosity, 0), quiet: this.profileForm.quiet === true,
        log_level: this.profileForm.log_level, log_format: this.profileForm.log_format,
        progress: this.profileForm.progress, output: this.profileForm.output,
        remote_log_url: this.profileForm.remote_log_url || null,
        remote_log_mode: this.profileForm.remote_log_mode
      };
    },
    secretOperations(profile) { return [["password", "password"], ["totp", "totp"], ["remote_log_token", "remote-log-token"]].filter(([field]) => this.secretModes[field] !== "keep").map(([field, kind]) => ({ profile, kind, mode: this.secretModes[field], value: this.secretModes[field] === "replace" ? this.secretValues[field] : null })); },
    validateSecretOperations(secrets) {
      if (!secrets.length) return "";
      if (!this.canManageSecrets) return "The security policy does not permit protected-secret changes.";
      if (secrets.some((item) => !["replace", "clear"].includes(item.mode))) return "Choose keep, replace, or clear for each protected secret.";
      if (secrets.some((item) => item.kind === "remote-log-token" && item.mode === "replace") && !this.canReplaceRemoteLogToken) return "The security policy does not permit replacing a remote-log token. You may still clear its stored value.";
      if (secrets.some((item) => item.mode === "replace" && !item.value)) return "Replacement secret values cannot be empty.";
      if (secrets.some((item) => item.mode === "replace" && (utf8ByteLength(item.value) > 4096 || /[\0\r\n]/.test(item.value)))) return "Replacement secrets must be one line and no more than 4096 bytes.";
      return "";
    },
    validateProfile(payload, secrets) {
      if (!/^[A-Za-z0-9_-]{1,64}$/.test(payload.name)) return "Name must use letters, digits, underscore, or hyphen.";
      if (!payload.source || !payload.url || !payload.username || !payload.remote) return "Name, source, URL, username, and remote path are required.";
      if (!validLocalSourcePath(payload.source)) return "Local source must be a canonical internal, USB, or SATA DSM volume path outside DSM-managed folders.";
      if (!validBoundedText(payload.url, 2048) || !(payload.url.startsWith("https://") || (payload.allow_http && payload.url.startsWith("http://")))) return "File Station URL must use HTTPS, or HTTP only with the controlled-LAN exception enabled.";
      if (!validBoundedText(payload.username, 256)) return "DSM username must be 1 through 256 bytes without control characters.";
      if (!validBoundedText(payload.remote, 247) || !payload.remote.startsWith("/") || payload.remote === "/" || payload.remote.endsWith("/") || payload.remote.includes("//") || hasDotPathSegment(payload.remote)) return "Remote path must be an absolute non-root File Station path without trailing, empty, or dot segments.";
      if (payload.excludes.length > 64 || payload.excludes.some((item) => !validBoundedText(item, 512))) return "Use at most 64 non-empty exclusion patterns of 512 bytes each.";
      if (payload.ca_certificate !== null && (!validBoundedText(payload.ca_certificate, 4096) || !payload.ca_certificate.startsWith("/") || hasDotPathSegment(payload.ca_certificate))) return "CA certificate must be an absolute NAS path without dot segments.";
      if (payload.danger_accept_invalid_certs && !this.profileForm.danger_invalid_confirm) return "Explicitly accept the TLS interception risk.";
      if (payload.remote_log_url && (!validBoundedText(payload.remote_log_url, 2048) || !payload.remote_log_url.startsWith("https://"))) return "Remote log delivery requires an HTTPS URL of at most 2048 bytes.";
      if (payload.remote_log_mode === "required" && !payload.remote_log_url) return "Required remote logging needs an HTTPS remote log URL.";
      if (payload.allow_empty_source && !payload.delete) return "Allowing an empty source requires deletion to be enabled and bounded for this profile.";
      if (!["content", "metadata", "size-only"].includes(payload.compare)) return "Choose a supported comparison mode.";
      if (!["trace", "debug", "info", "warn", "error", "off"].includes(payload.log_level)) return "Choose a supported log level.";
      if (!["human", "json"].includes(payload.log_format)) return "Choose human-readable or JSON log format.";
      if (!["auto", "always", "never"].includes(payload.progress)) return "Choose a supported progress mode.";
      if (!["human", "json", "ndjson"].includes(payload.output)) return "Choose human-readable, JSON, or newline-delimited JSON output.";
      if (!["best-effort", "required"].includes(payload.remote_log_mode)) return "Choose a supported remote log mode.";
      if (payload.allow_http && !this.canAllowHttp) return "The security policy does not permit HTTP destinations.";
      if (payload.allow_empty_source && !this.canAllowEmptySource) return "The security policy does not permit empty-source exceptions.";
      if (payload.danger_accept_invalid_certs && !this.canAllowInvalidTls) return "The security policy does not permit invalid TLS certificates.";
      if (payload.delete && !this.canAllowDestructive) return "The security policy does not permit deletion-capable profiles.";
      if (payload.remote_log_url && !this.canAllowRemoteLogging) return "The security policy does not permit remote logging.";
      const secretError = this.validateSecretOperations(secrets);
      if (secretError) return secretError;
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
      if (!this.profileEditorOpen) return;
      if (scopeMutationOutcomeUnresolved(this, "profile")) return this.toast("Profile save locked", scopeMutationGuidance(this, "profile"), true);
      if (!this.canChangeProfiles || this.operationBusy || this.profileSaveState === "saving") return this.toast("Profile save unavailable", "Wait for the active package operation or restore the authenticated mutation bridge, then try again.", true);
      this.cancelAutosave("profile");
      const payload = this.profileAutosavePayload();
      if (!payload) return this.toast("Profile not saved", "Finish every numeric profile value with a whole number before saving.", true);
      const secrets = this.secretOperations(payload.name); const error = this.validateProfile(payload, secrets);
      if (error) return this.toast("Profile not saved", error, true);
      if (!this.selectedProfile && !secrets.some((secret) => secret.kind === "password" && secret.mode === "replace")) return this.toast("Profile not saved", "A new profile requires a password. Choose Replace securely, enter it, and save again.", true);
      const risky = payload.allow_http || payload.allow_empty_source || payload.danger_accept_invalid_certs || payload.delete;
      if (risky && !await this.confirmAction("Save dangerous profile settings?", "Review plain-HTTP, deletion, empty-source, and TLS settings before continuing.", "Save profile")) return;
      const creatingProfile = !this.selectedProfile;
      const creationStageTotal = secrets.length + 2;
      this.profileSaveState = "saving";
      if (creatingProfile) this.setProfileCreationStage(1, creationStageTotal, "Validating the local source with the package identity…");
      else {
        this.clearProfileCreationProgress();
        this.profileSaveMessage = "Validating the local source with the package identity…";
      }
      this.toast(
        creatingProfile ? "Profile creation started" : "Profile save started",
        creatingProfile
          ? `Validating the local source before creating the profile and applying its protected credentials. ${PROFILE_CREATION_WINDOW_WARNING}`
          : "Validating the local source before saving profile changes and applying explicit protected credential operations."
      );
      this.operationBusy = true;
      let configurationApplied = false;
      let activeSecretKind = "";
      const appliedSecretKinds = [];
      try {
        let sourceValidation;
        try {
          sourceValidation = await apiGet(this.auth, "source-path", { path: payload.source }, AUTOSAVE_API_LIMITS);
        } catch (_sourceError) {
          throw new Error("Local source validation failed. Confirm the folder exists, is a canonical non-symlink DSM volume path, is not DSM-managed, and the package identity can read and traverse it.");
        }
        if (sourceValidation.schema !== "sdsync.dsm-source-path.v1" || sourceValidation.path !== payload.source || sourceValidation.valid !== true) throw new Error("The package could not validate the exact local source path.");
        if (this.disposed) return;
        if (creatingProfile) this.setProfileCreationStage(2, creationStageTotal, "Queueing and applying the profile configuration…");
        else this.profileSaveMessage = "Saving profile configuration…";
        await apiPost(this.auth, this.csrfToken, ACTIONS.configureProfile, payload, true, undefined, AUTOSAVE_API_LIMITS);
        configurationApplied = true;
        this.clearProfileConfigurationFailure(false);
        if (this.disposed) return;
        for (const [secretIndex, secret] of secrets.entries()) {
          activeSecretKind = secret.kind;
          const secretLabel = secret.kind === "remote-log-token" ? "remote log token" : secret.kind;
          if (creatingProfile) this.setProfileCreationStage(secretIndex + 3, creationStageTotal, `Queueing and applying the protected ${secretLabel} credential…`);
          else this.profileSaveMessage = `Applying protected ${secret.kind} operation…`;
          const secretResult = await apiPost(this.auth, this.csrfToken, ACTIONS.setSecret, secret, true, undefined, AUTOSAVE_API_LIMITS);
          this.applyTrustedSecretPresence(secretResult);
          appliedSecretKinds.push(secret.kind);
          const appliedField = secret.kind === "remote-log-token" ? "remote_log_token" : secret.kind;
          if (this.secretModes && Object.prototype.hasOwnProperty.call(this.secretModes, appliedField)) this.secretModes[appliedField] = "keep";
          if (this.secretValues && Object.prototype.hasOwnProperty.call(this.secretValues, appliedField)) this.secretValues[appliedField] = "";
          activeSecretKind = "";
          if (this.disposed) return;
        }
        this.hydrateAutosave("profile", payload);
        this.profileSaveState = "success";
        this.profileSaveMessage = "Profile saved successfully.";
        this.toast("Profile saved", "The controller applied the validated configuration and protected credential operations.");
        this.clearSecrets();
        this.closeProfile({ refresh: false });
        const observed = await this.refreshSnapshot(false, true);
        if (!this.disposed && observed === true) {
          this.clearProfileSecretFailures(appliedSecretKinds);
        } else if (!this.disposed) {
          this.refreshAutosaveStatus();
        }
      } catch (caught) {
        if (this.disposed) return;
        const partiallyApplied = configurationApplied || appliedSecretKinds.length > 0;
        const reportedError = partiallyApplied
          ? partialMutationInspectionRequired(caught, "A later profile stage failed.", "Earlier profile stages were applied.")
          : caught;
        this.pauseAutosave("profile", reportedError, activeSecretKind, {
          expectedConfiguration: payload,
          creatingProfile
        });
        this.reportMutationError(
          reportedError,
          partiallyApplied ? "Profile partially applied" : "Profile not saved",
          partiallyApplied ? "Profile partially applied · inspect state" : "Profile outcome unknown",
          "The package rejected the change."
        );
        if (configurationApplied && !this.selectedProfile) this.selectedProfile = payload.name;
        this.profileSaveState = "error";
        this.profileSaveMessage = partiallyApplied || caught.outcomeUnknown === true
          ? "The profile editor was preserved, but the outcome needs Activity / Logs inspection before another save."
          : boundedText(caught && caught.message, "The package rejected the profile. Correct the draft and try again.");
      } finally {
        if (!this.disposed) {
          this.clearProfileCreationProgress();
          this.operationBusy = false;
        }
      }
    },
    async saveProfileSecrets(event) {
      if (event && event.preventDefault) event.preventDefault();
      const profile = this.selectedProfile;
      if (scopeMutationOutcomeUnresolved(this, "profile")) return this.toast("Secret save locked", scopeMutationGuidance(this, "profile"), true);
      if (!profile || !this.canManageSecrets || this.operationBusy) return;
      const secrets = this.secretOperations(profile);
      if (!secrets.length) return this.toast("No secret changes", "Choose Replace securely or Clear stored value for at least one protected secret.");
      const error = this.validateSecretOperations(secrets);
      if (error) return this.toast("Secrets not saved", error, true);
      if (secrets.some((item) => item.mode === "clear")
        && !await this.confirmAction("Clear stored profile secrets?", "Only the selected password, TOTP, or remote-log token values will be removed. Profile configuration remains unchanged.", "Clear selected secrets")) return;
      this.profileSaveState = "saving";
      this.profileSaveMessage = "Applying changed protected credentials…";
      this.toast("Saving changed secrets", "Applying the selected protected credential operations in order.");
      this.operationBusy = true;
      let activeSecretKind = "";
      const appliedSecretKinds = [];
      try {
        for (const secret of secrets) {
          activeSecretKind = secret.kind;
          this.profileSaveMessage = `Applying protected ${secret.kind} operation…`;
          const secretResult = await apiPost(this.auth, this.csrfToken, ACTIONS.setSecret, secret, true, undefined, AUTOSAVE_API_LIMITS);
          this.applyTrustedSecretPresence(secretResult);
          appliedSecretKinds.push(secret.kind);
          const appliedField = secret.kind === "remote-log-token" ? "remote_log_token" : secret.kind;
          if (this.secretModes && Object.prototype.hasOwnProperty.call(this.secretModes, appliedField)) this.secretModes[appliedField] = "keep";
          if (this.secretValues && Object.prototype.hasOwnProperty.call(this.secretValues, appliedField)) this.secretValues[appliedField] = "";
          activeSecretKind = "";
          if (this.disposed) return;
        }
        this.profileSaveState = "success";
        this.profileSaveMessage = "Changed protected credentials saved successfully.";
        this.toast("Secrets saved", "The package applied and audited only the selected protected-secret operations.");
        this.clearProfileSecretFailures(appliedSecretKinds);
        this.refreshAutosaveStatus();
      } catch (caught) {
        if (this.disposed) return;
        const partiallyApplied = appliedSecretKinds.length > 0;
        const reportedError = partiallyApplied
          ? partialMutationInspectionRequired(caught, "A later secret stage failed.", "Earlier secret stages were applied.")
          : caught;
        if (partiallyApplied || caught.outcomeUnknown === true) {
          this.pauseAutosave("profile", reportedError, activeSecretKind);
        }
        this.reportMutationError(
          reportedError,
          partiallyApplied ? "Secrets partially applied" : "Secrets not saved",
          partiallyApplied ? "Secrets partially applied · inspect state" : "Secret outcome unknown",
          "The package rejected the protected-secret operation."
        );
        this.profileSaveState = "error";
        this.profileSaveMessage = partiallyApplied || caught.outcomeUnknown === true
          ? "Applied secret stages were cleared; unapplied draft values remain available while Activity / Logs are inspected."
          : boundedText(caught && caught.message, "The package rejected the secret change. Correct the draft and try again.");
        this.refreshAutosaveStatus();
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    async removeProfile() {
      if (scopeMutationOutcomeUnresolved(this, "profile")) return this.toast("Profile deletion locked", scopeMutationGuidance(this, "profile"), true);
      if (!this.canChangeProfiles || !this.selectedProfile || this.operationBusy) return;
      const name = this.selectedProfile;
      if (!await this.confirmAction(`Delete profile ${name}?`, "This removes package-owned configuration and protected credentials. Synced files are not deleted.", "Delete profile")) return;
      this.operationBusy = true;
      try {
        await apiPost(this.auth, this.csrfToken, ACTIONS.removeProfile, { name });
        if (this.disposed) return;
        this.clearAutosaveFailure("profile");
        this.toast("Profile deleted", `The controller removed ${name} and its stored credentials.`);
        this.closeProfile({ refresh: false });
        await this.refreshSnapshot(false, true);
      } catch (error) {
        if (this.disposed) return;
        this.pauseAutosave("profile", error);
        this.reportMutationError(error, "Profile not deleted", "Profile deletion outcome unknown", "The package rejected the change.");
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    loadRoutine(profileName) { const profile = typeof profileName === "string" ? profileName : this.routineForm.profile; const routine = this.routines.find((item) => String(item.profile) === String(profile)); this.routineForm = routine ? { profile, enabled: routine.enabled === true, action: routine.action || "sync", mode: routine.mode || "interval", interval_seconds: numberOr(routine.interval_seconds, 3600), weekdays: Array.isArray(routine.weekdays) ? routine.weekdays.map(Number) : String(routine.weekdays || "1,2,3,4,5,6,7").split(",").map(Number), time_window_start: routine.time_window_start || routine.window_start || "00:00", time_window_end: routine.time_window_end || routine.window_end || "23:59", debounce_seconds: numberOr(routine.debounce_seconds, 45), poll_seconds: numberOr(routine.poll_seconds, 30), retry_count: numberOr(routine.retry_count, 5), retry_backoff_seconds: numberOr(routine.retry_backoff_seconds, 60), retry_exponential: routine.retry_exponential !== false, allow_delete: routine.allow_delete === true, max_total_delete: numberOr(routine.max_total_delete, 100), depends_on: arrayOf(routine.depends_on).map(String) } : emptyRoutine(profile); this.hydrateAutosave("routine", this.routinePayload(), false); if (this.autosaveCoordinator) this.autosaveCoordinator.setScopeBlocked("routine", this.autosaveFailureScopes.routine || !routine); },
    openRoutine(profile = "") { if (this.operationBusy || (!profile && !this.canChangeRoutines)) return; this.routineEditorOpen = true; this.loadRoutine(profile); },
    closeRoutine() { this.cancelAutosave("routine"); this.routineEditorOpen = false; this.routineForm = emptyRoutine(); },
    routinePayload() {
      const payload = { profile: this.routineForm.profile, enabled: this.routineForm.enabled === true, action: this.routineForm.action, mode: this.routineForm.mode, retry_count: this.integer(this.routineForm.retry_count, 5), retry_backoff_seconds: this.integer(this.routineForm.retry_backoff_seconds, 60), retry_exponential: this.routineForm.retry_exponential !== false, allow_delete: this.routineForm.allow_delete === true, max_total_delete: this.integer(this.routineForm.max_total_delete, 100), depends_on: this.routineForm.depends_on.map(String) };
      if (payload.mode === "interval") payload.interval_seconds = this.integer(this.routineForm.interval_seconds, 3600);
      else if (payload.mode === "daily") Object.assign(payload, { weekdays: this.routineForm.weekdays.map(Number), time_window_start: this.routineForm.time_window_start, time_window_end: this.routineForm.time_window_end });
      else if (payload.mode === "realtime") Object.assign(payload, { debounce_seconds: this.integer(this.routineForm.debounce_seconds, 45), poll_seconds: this.integer(this.routineForm.poll_seconds, 30) });
      return payload;
    },
    async saveRoutine(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (scopeMutationOutcomeUnresolved(this, "profile")) return this.toast("Routine save locked", scopeMutationGuidance(this, "profile"), true);
      if (scopeMutationOutcomeUnresolved(this, "routine")) return this.toast("Routine save locked", scopeMutationGuidance(this, "routine"), true);
      if (!this.canChangeRoutines || !this.routineForm.profile || this.operationBusy) return;
      this.cancelAutosave("routine");
      const payload = this.routineAutosavePayload();
      if (!payload) return this.toast("Routine not saved", "Finish every numeric routine value with a whole number before saving.", true);
      const invalid = this.validateRoutinePayload(payload);
      if (invalid) return this.toast("Routine not saved", invalid, true);
      if (this.routineAutosaveNeedsReview(payload) && payload.allow_delete && !await this.confirmAction("Enable routine deletion?", "This routine may apply the profile's separately bounded deletion policy. Review both deletion ceilings before continuing.", "Enable routine deletion")) return;
      this.operationBusy = true;
      try {
        await apiPost(this.auth, this.csrfToken, ACTIONS.routine, payload);
        if (this.disposed) return;
        this.hydrateAutosave("routine", payload);
        this.toast("Routine saved", "The controller applied the per-profile policy.");
        await this.refreshSnapshot(false, true);
        if (!this.disposed) this.loadRoutine(payload.profile);
      } catch (error) {
        if (this.disposed) return;
        this.pauseAutosave("routine", error);
        this.reportMutationError(error, "Routine not saved", "Routine outcome unknown", "The package rejected the routine.");
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    async removeRoutine() {
      const profile = this.routineForm.profile;
      if (scopeMutationOutcomeUnresolved(this, "profile")) return this.toast("Routine removal locked", scopeMutationGuidance(this, "profile"), true);
      if (scopeMutationOutcomeUnresolved(this, "routine")) return this.toast("Routine removal locked", scopeMutationGuidance(this, "routine"), true);
      if (!this.canChangeRoutines || !profile || !this.selectedRoutine || this.operationBusy) return;
      if (!await this.confirmAction(`Remove routine for ${profile}?`, "The profile remains configured, but package automation for it will stop.", "Remove routine")) return;
      this.operationBusy = true;
      try {
        await apiPost(this.auth, this.csrfToken, ACTIONS.removeRoutine, { name: profile });
        if (this.disposed) return;
        this.clearAutosaveFailure("routine");
        this.toast("Routine removed", `The controller removed automation for ${profile}.`);
        this.closeRoutine();
        await this.refreshSnapshot(false, true);
      } catch (error) {
        if (this.disposed) return;
        this.pauseAutosave("routine", error);
        this.reportMutationError(error, "Routine not removed", "Routine removal outcome unknown", "The package rejected the change.");
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    async saveAlerts(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (scopeMutationOutcomeUnresolved(this, "alerts")) return this.toast("Alert policy save locked", scopeMutationGuidance(this, "alerts"), true);
      if (!this.canChangeNotifications || this.operationBusy) return;
      this.cancelAutosave("alerts");
      const payload = this.alertPayload();
      const invalid = this.validateAlertPayload(payload);
      if (invalid) return this.toast("Alert policy not saved", invalid, true);
      this.operationBusy = true;
      try {
        await apiPost(this.auth, this.csrfToken, ACTIONS.alertPolicy, payload);
        if (this.disposed) return;
        this.alertDirty = false;
        this.hydrateAutosave("alerts", payload);
        this.toast("Alert policy saved", "The controller applied the DSM desktop alert policy.");
        await this.refreshSnapshot(false, true);
      } catch (error) {
        if (this.disposed) return;
        this.pauseAutosave("alerts", error);
        this.reportMutationError(error, "Alert policy not saved", "Alert policy outcome unknown", "The package rejected the policy.");
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    async executeOperation(kind, payload) {
      if (scopeMutationOutcomeUnresolved(this, "profile")) return this.toast("Operation locked", scopeMutationGuidance(this, "profile"), true);
      if (isolatedIncidentUnresolved(this, "operations")) return this.toast("Operation locked", isolatedIncidentGuidance(this, "operations"), true);
      if (!this.canRunOperations || this.operationBusy || this.disposed) return;
      if (payload && payload.allow_delete === true && !this.canAllowDestructive) return;
      if (kind === "doctor" && payload && payload.write_test === true && (!this.canRunDoctorWrite || !this.hasCapability("write_test"))) return;
      const doctorLevel = normalizedDoctorLevel(payload && payload.level);
      const doctorStartedEpoch = Math.floor(Date.now() / 1000);
      if (kind === "doctor") {
        this.doctorProgress = { active: true, phase: "execution", level: doctorLevel, started_epoch: doctorStartedEpoch };
        this.doctorReport = runningDoctorReport(doctorLevel, payload && payload.write_test === true, doctorStartedEpoch);
        this.diagnostic = { title: "Doctor running", output: "Waiting for bounded terminal evidence from the package controller…" };
      }
      this.operationBusy = true;
      const awaitTerminal = kind === "doctor";
      try {
        const result = await apiPost(
          this.auth,
          this.csrfToken,
          ACTIONS.execute,
          Object.assign({ kind }, payload),
          awaitTerminal,
          undefined,
          undefined,
          // Only the awaited path polls, so only it can observe anything. A
          // queued-and-forgotten operation has no reader to publish to.
          awaitTerminal ? openProgressSink(this, kind) : null
        );
        if (this.disposed) return;
        const message = boundedText(
          result.output || result.message,
          awaitTerminal
            ? "Doctor completed without additional output."
            : "Queued safely; follow Activity and Logs for the final result."
        );
        let operationMessage = message;
        if (awaitTerminal) {
          this.doctorReport = doctorReportFromResult(result, true, doctorLevel, payload && payload.write_test === true, doctorStartedEpoch);
          operationMessage = doctorDisplayOutput(this.doctorReport, message);
          this.diagnostic = { title: "Doctor completed", output: operationMessage };
          this.doctorProgress = { active: false, phase: "complete", level: doctorLevel, started_epoch: doctorStartedEpoch };
        }
        const operation = `${kind.charAt(0).toUpperCase()}${kind.slice(1)}`;
        this.toast(`${operation} ${awaitTerminal ? "completed" : "queued"}`, operationMessage);
        await this.refreshSnapshot(false, true);
      } catch (error) {
        if (this.disposed) return;
        const operation = `${kind.charAt(0).toUpperCase()}${kind.slice(1)}`;
        const report = this.reportMutationError(
          error,
          `${operation} failed`,
          `${operation} outcome unknown`,
          "The package rejected the operation."
        );
        recordIsolatedIncident(this, "operations", operation, error, report, { subject: boundedText(payload && payload.scope, operation) });
        if (kind === "doctor") {
          this.doctorReport = doctorReportFromResult(
            { output: error.resultOutput || report.message, message: report.message },
            false,
            doctorLevel,
            payload && payload.write_test === true,
            doctorStartedEpoch
          );
          this.diagnostic = {
            title: report.unknown ? "Doctor outcome unknown" : "Doctor failed",
            output: doctorDisplayOutput(this.doctorReport, error.resultOutput || report.message)
          };
          this.doctorProgress = { active: false, phase: report.unknown ? "unknown" : "failed", level: doctorLevel, started_epoch: doctorStartedEpoch };
        }
      } finally {
        if (!this.disposed) {
          this.operationBusy = false;
          closeProgressSink(this);
          if (kind === "doctor" && this.doctorProgress.active) this.doctorProgress = { active: false, phase: "unknown", level: doctorLevel, started_epoch: doctorStartedEpoch };
        }
      }
    },
    quickPlan() { return this.executeOperation("plan", { scope: "all", level: null, write_test: null, allow_delete: false, max_total_delete: 0 }); },
    async quickRun() { if (!this.canRunOperations || this.operationBusy) return; if (await this.confirmAction("Run all configured profiles?", "This starts a real one-way sync. Remote deletion stays disabled for this quick action.", "Run all")) return this.executeOperation("run", { scope: "all", level: null, write_test: null, allow_delete: false, max_total_delete: 0 }); },
    async runDoctor(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (!this.canRunOperations || this.operationBusy) return;
      const level = normalizedDoctorLevel(this.doctorForm.level);
      if (this.doctorForm.write_test && level !== "extensive") return this.toast("Extensive level required", "Disposable write verification is available only after the full Extensive read-only checks.", true);
      if (this.doctorForm.write_test && (!this.canRunDoctorWrite || !this.hasCapability("write_test"))) return this.toast("Doctor write test blocked", "The package capability or security policy does not permit disposable destination probes.", true);
      if (this.doctorForm.write_test && !this.doctorForm.write_confirm) return this.toast("Write-test confirmation required", "Approve the disposable probe and cleanup before running.", true);
      if (this.doctorForm.write_test && !await this.confirmAction("Run the disposable target probe?", "The doctor briefly creates, verifies, and removes a unique probe in the selected destination after Extensive read-only checks.", "Run write test")) return;
      return this.executeOperation("doctor", { scope: this.doctorForm.scope, level, write_test: this.doctorForm.write_test, allow_delete: null, max_total_delete: null });
    },
    syncStateClass(state) { return ["sdsync-sync-state", `is-${String(state || "unknown")}`]; },
    syncEntrySide(size, epoch) { return size === null ? "Absent" : `${formatBytes(size)} · ${formatDate(epoch)}`; },
    resyncEntryLabel(entry) { return `Plan a re-upload of ${entry.relative}`; },
    setSyncField(field, value) {
      this.syncStatusForm[field.key] = value;
      // Changing the profile also invalidates any resync plan, whose ticket
      // was computed against the other profile's destination.
      if (field.resets) this.onSyncProfileChanged();
    },
    resetSyncStatus(message) {
      this.syncStatusResult = emptySyncStatusPage();
      this.syncStatusCursors = [];
      this.syncStatusPageNumber = 0;
      this.syncStatusPhase = "idle";
      this.syncStatusMessage = boundedText(message, SYNC_STATUS_IDLE_MESSAGE);
    },
    // A loaded page belongs to the query that produced it. Changing the query
    // discards it rather than leaving rows on screen under a heading that no
    // longer describes them.
    onSyncQueryChanged() {
      if (this.syncStatusResult.loaded || this.syncStatusPhase === "failed") {
        this.resetSyncStatus("Query changed. Check status again to reload.");
      }
    },
    onSyncProfileChanged() {
      this.onSyncQueryChanged();
      this.clearResyncPlan("");
    },
    clearSyncFilters() {
      this.syncStatusForm.scope = "";
      this.syncStatusForm.filter = "";
      this.syncStatusForm.state = "attention";
      this.syncStatusForm.include_excluded = false;
      this.onSyncQueryChanged();
    },
    // The cheap half of this page. A GET, not a queued mutation: it opens the
    // small documents the last walk stored and touches no source file and no
    // File Station endpoint, so it answers in milliseconds and must never wait
    // behind the walk it exists to save the operator from running.
    async refreshStatusRollup() {
      if (this.disposed || this.statusRollupLoading) return;
      this.statusRollupLoading = true;
      try {
        const model = await apiGet(this.auth, "status-rollup");
        if (this.disposed) return;
        const rollup = normalizedStatusRollup(model);
        this.statusRollup = rollup;
        if (!rollup.loaded) this.statusRollupMessage = UNREADABLE_RESPONSE;
        else if (!rollup.total && !rollup.profiles.length) this.statusRollupMessage = ROLLUP_IDLE_MESSAGE;
      } catch (error) {
        if (this.disposed) return;
        this.statusRollup = emptyStatusRollup();
        this.statusRollupMessage = boundedText(
          this.describeBridgeError(error, "status").message,
          "The stored totals could not be read."
        );
      } finally {
        if (!this.disposed) this.statusRollupLoading = false;
      }
    },
    async loadSyncStatus(cursor, pageNumber) {
      if (!this.syncStatusReady || this.disposed) return;
      const profile = boundedText(this.syncStatusForm.profile, "");
      const requested = Number(this.syncStatusForm.limit);
      const limit = SYNC_PAGE_SIZES.includes(requested) ? requested : SYNC_PAGE_SIZE_DEFAULT;
      const page = Math.max(1, Number(pageNumber) || 1);
      const from = boundedText(cursor, "");
      const query = {
        state: boundedText(this.syncStatusForm.state, "attention"),
        filter: boundedText(this.syncStatusForm.filter, "").trim(),
        scope: boundedText(this.syncStatusForm.scope, "").trim(),
        includeExcluded: this.syncStatusForm.include_excluded === true
      };
      this.syncStatusBusy = true;
      this.syncStatusPhase = "loading";
      this.syncStatusMessage = "Comparing both sides for this scope. The NAS is only read; a large scope takes a while.";
      try {
        // A queued job, not a read: this walks the local tree and enumerates
        // the remote one, so it goes through the same terminal-observation
        // path as Doctor rather than occupying a synchronous bridge worker.
        const result = await apiPost(this.auth, this.csrfToken, ACTIONS.syncStatus, {
          cursor: from,
          filter: query.filter,
          include_excluded: query.includeExcluded,
          limit: Math.min(limit, SYNC_STATUS_MAX_LIMIT),
          profile,
          scope: query.scope,
          state: query.state
        }, true, undefined, undefined, openProgressSink(this, ACTIONS.syncStatus));
        if (this.disposed) return;
        const document = bridgeDocument(result, "sdsync.status.v1");
        if (!document) throw new Error(UNREADABLE_RESPONSE);
        this.syncStatusResult = Object.assign(syncStatusPage(document, profile), { query });
        this.syncStatusPageNumber = page;
        // One cursor per page already visited, so Previous can return to an
        // exact position instead of re-walking from the start.
        this.syncStatusCursors = this.syncStatusCursors.slice(0, page - 1).concat(from);
        this.syncStatusPhase = "ready";
        this.syncStatusMessage = this.syncStatusResult.entries.length
          ? ""
          : "No entry matches this state and search. The totals still cover the whole scope.";
      } catch (error) {
        if (this.disposed) return;
        // Classified like any queued operation, but deliberately not recorded
        // as an isolated incident: an unknown outcome here left nothing behind
        // to reconcile, and quarantining every operation because a read-only
        // scan did not report back would be the wrong trade.
        const report = this.reportMutationError(
          error,
          "Sync status failed",
          "Sync status outcome unknown",
          "The package could not complete the status query."
        );
        this.syncStatusResult = emptySyncStatusPage();
        this.syncStatusCursors = [];
        this.syncStatusPageNumber = 0;
        this.syncStatusPhase = "failed";
        this.syncStatusMessage = report.message;
      } finally {
        if (!this.disposed) {
          this.syncStatusBusy = false;
          closeProgressSink(this);
        }
      }
    },
    checkSyncStatus(event) {
      if (event && event.preventDefault) event.preventDefault();
      return this.loadSyncStatus("", 1);
    },
    syncStatusNextPage() {
      if (!this.syncStatusHasNext) return undefined;
      return this.loadSyncStatus(this.syncStatusResult.nextCursor, this.syncStatusPageNumber + 1);
    },
    syncStatusPreviousPage() {
      if (!this.syncStatusHasPrevious) return undefined;
      return this.loadSyncStatus(this.syncStatusCursors[this.syncStatusPageNumber - 2] || "", this.syncStatusPageNumber - 1);
    },
    clearResyncPlan(message) {
      this.resyncPlan = emptyResyncPlan();
      this.resyncPhase = "idle";
      this.resyncMessage = boundedText(message, "");
    },
    resyncEntry(entry) {
      if (!entry || !this.resyncCanPlan) return undefined;
      this.resyncForm.scope = boundedText(entry.relative, "");
      return this.planResync();
    },
    // Both phases are the same request with and without a ticket, so they are
    // one method: the difference that matters is that a ticket is only ever
    // sent after a person confirmed the exact plan it was printed with.
    async runResync(ticket, profile, scope) {
      const confirming = Boolean(ticket);
      this.resyncBusy = true;
      this.resyncPhase = confirming ? "confirming" : "planning";
      this.resyncMessage = confirming
        ? "Re-uploading the confirmed files. Keep this AppWindow open."
        : "Building the overwrite list. This step changes nothing.";
      try {
        const result = await apiPost(this.auth, this.csrfToken, ACTIONS.resync, { confirm: ticket, profile, scope }, true, undefined, undefined, openProgressSink(this, ACTIONS.resync));
        if (this.disposed) return;
        const document = bridgeDocument(result, "sdsync.resync.v1");
        if (!document) throw new Error(UNREADABLE_RESPONSE);
        const plan = Object.assign(resyncPlanFromDocument(document, profile), { requestedScope: scope });
        this.resyncPlan = plan;
        if (plan.confirmed) {
          this.resyncPhase = "confirmed";
          this.resyncMessage = `Re-uploaded ${plan.overwrites} file${plan.overwrites === 1 ? "" : "s"} to ${profile}.`;
          this.toast("Re-upload complete", this.resyncMessage);
          await this.refreshSnapshot(false, true);
          return;
        }
        this.resyncPhase = plan.overwrites && plan.ticket ? "planned" : "empty";
        if (!plan.overwrites) {
          this.resyncMessage = "Nothing would be re-uploaded in this scope.";
        } else if (confirming) {
          // The engine refused a confirmation that no longer described what
          // would be overwritten, and re-planned in the same response, so the
          // window shows the replacement instead of looping back to planning.
          this.resyncMessage = "What would be overwritten changed, so nothing was uploaded. Confirm the refreshed list to accept it.";
          this.toast("Resync plan changed", this.resyncMessage, true);
        } else {
          this.resyncMessage = "Review this exact list, then confirm it. Nothing has changed yet.";
        }
      } catch (error) {
        if (this.disposed) return;
        const report = this.reportMutationError(
          error,
          confirming ? "Resync failed" : "Resync plan failed",
          confirming ? "Resync outcome unknown" : "Resync plan outcome unknown",
          "The package rejected the re-upload."
        );
        recordIsolatedIncident(this, "operations", "Resync", error, report, { subject: scope || profile });
        this.resyncPhase = report.unknown ? "unknown" : "failed";
        this.resyncMessage = report.message;
      } finally {
        if (!this.disposed) {
          this.resyncBusy = false;
          closeProgressSink(this);
        }
      }
    },
    // Planning never writes. It exists to produce the exact overwrite list and
    // the ticket that proves that list was the one displayed.
    planResync(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (!this.resyncCanPlan || this.disposed) return undefined;
      return this.runResync("", boundedText(this.syncStatusForm.profile, ""), boundedText(this.resyncForm.scope, "").trim());
    },
    async confirmResync() {
      if (!this.resyncPlanReady || this.disposed) return;
      const plan = this.resyncPlan;
      const count = plan.overwrites;
      if (!await this.confirmAction(
        `Re-upload ${count} file${count === 1 ? "" : "s"}?`,
        `Every listed file is overwritten whatever the remote copy holds now. ${formatBytes(plan.overwriteBytes)} will be uploaded. Nothing is deleted.`,
        "Re-upload now"
      )) return;
      // Re-checked after the dialog: the plan may have been discarded, or a
      // profile change may have invalidated it, while it was open.
      if (!this.resyncPlanReady || this.disposed || this.resyncPlan !== plan) return;
      await this.runResync(plan.ticket, boundedText(plan.profile, ""), boundedText(plan.requestedScope, ""));
    },
    activityEvidence(event) { return activityTroubleshootingText(event); },
    logEvidence(record) {
      if (!record || typeof record !== "object") return "";
      const source = typeof record.troubleshootingSource === "string"
        ? record.troubleshootingSource
        : troubleshootingField(record.source, "log");
      const text = typeof record.troubleshootingText === "string"
        ? record.troubleshootingText
        : redactedTroubleshootingText(typeof record.text === "string" ? record.text : "");
      return boundedSanitizedTroubleshootingText([
        "Synology Drive Sync package log",
        `Source: ${source}`,
        `Visible lines: ${Math.max(0, Number(record.lineCount) || 0)}`,
        "",
        text
      ].join("\n"), TROUBLESHOOTING_RECORD_LIMIT);
    },
    async writeTroubleshootingClipboard(text) {
      const browserWindow = typeof window === "object" ? window : null;
      const browserDocument = typeof document === "object" ? document : null;
      const clipboard = browserWindow && browserWindow.navigator && browserWindow.navigator.clipboard;
      if (clipboard && typeof clipboard.writeText === "function") {
        try {
          await clipboard.writeText(text);
          return;
        } catch (_error) { /* Fall through to the synchronous DSM-compatible path. */ }
      }
      if (!browserDocument || !browserDocument.body || typeof browserDocument.createElement !== "function") {
        throw new Error("Clipboard API unavailable");
      }
      const active = browserDocument.activeElement;
      const textarea = browserDocument.createElement("textarea");
      textarea.value = text;
      textarea.setAttribute("readonly", "");
      textarea.setAttribute("aria-hidden", "true");
      textarea.style.cssText = "position:fixed;left:-10000px;top:0;width:1px;height:1px;opacity:0;pointer-events:none";
      browserDocument.body.appendChild(textarea);
      try {
        textarea.focus();
        textarea.select();
        if (typeof textarea.setSelectionRange === "function") textarea.setSelectionRange(0, textarea.value.length);
        if (typeof browserDocument.execCommand !== "function" || browserDocument.execCommand("copy") !== true) {
          throw new Error("Clipboard copy was rejected");
        }
      } finally {
        if (textarea.parentNode) textarea.parentNode.removeChild(textarea);
        if (active && typeof active.focus === "function") active.focus();
      }
    },
    async copyTroubleshootingText(value, label, limit = TROUBLESHOOTING_RECORD_LIMIT, sanitized = false) {
      const text = sanitized
        ? boundedSanitizedTroubleshootingText(value, limit)
        : sanitizedTroubleshootingText(value, limit);
      const subject = troubleshootingField(label, "Evidence");
      if (!text) {
        this.toast("Nothing to copy", `${subject} has no visible troubleshooting evidence yet.`, true);
        return false;
      }
      try {
        await this.writeTroubleshootingClipboard(text);
        this.toast(`${subject} copied`, "Known DSM session and credential field shapes were redacted. Review the bounded text before sharing.");
        return true;
      } catch (_error) {
        this.toast("Copy failed", "DSM or the browser denied clipboard access. Select the visible evidence and copy it manually.", true);
        return false;
      }
    },
    copyActivityEvent(event) {
      return this.copyTroubleshootingText(
        this.activityEvidence(event),
        "Activity event",
        TROUBLESHOOTING_RECORD_LIMIT,
        true
      );
    },
    copyVisibleActivity() {
      const events = this.reversedActivity;
      const header = [
        "Synology Drive Sync filtered activity",
        `Visible events: ${events.length}`,
        `Category filter: ${troubleshootingField(this.activityCategory, "all")}`,
        `Level filter: ${troubleshootingField(this.activityLevel, "all")}`,
        `Search filter: ${troubleshootingField(this.activitySearch, "none")}`
      ].join("\n");
      const body = events.map((event, index) => `\n--- Event ${index + 1} ---\n${this.activityEvidence(event)}`).join("\n");
      return this.copyTroubleshootingText(
        `${header}\n${body}`,
        "Visible activity",
        TROUBLESHOOTING_VISIBLE_LIMIT,
        true
      );
    },
    copyLogRecord(record) {
      return this.copyTroubleshootingText(
        this.logEvidence(record),
        `${troubleshootingField(record && record.source, "Log")} log`,
        TROUBLESHOOTING_RECORD_LIMIT,
        true
      );
    },
    copyVisibleLogs() {
      const header = [
        "Synology Drive Sync visible package logs",
        `Source filter: ${troubleshootingField(this.logSource, "all")}`,
        `Line limit: ${Math.min(1000, Math.max(1, Number(this.logLines) || 200))}`,
        `Visible records: ${this.logRecords.length}`
      ].join("\n");
      const body = this.logRecords.map((record, index) => `\n--- Log record ${index + 1} ---\n${this.logEvidence(record)}`).join("\n");
      return this.copyTroubleshootingText(
        `${header}\n${body}`,
        "Visible logs",
        TROUBLESHOOTING_VISIBLE_LIMIT,
        true
      );
    },
    logRecordsFrom(model) {
      const records = [];
      let remaining = MAX_RESPONSE_BYTES;
      // A source selected on its own is rendered even when it has nothing to
      // show, so its Clear control stays reachable. An empty view does not mean
      // an empty file: raising a category's log level hides existing records
      // from this list without removing the bytes they occupy. Under "All logs"
      // the empty entries are dropped instead, so a fresh install still reads
      // as one "No log data yet." rather than six empty cards.
      const allowEmpty = this.logSource !== "all";
      const append = (sourceValue, textValue, structured = false) => {
        if (remaining <= 0) return;
        const source = troubleshootingField(sourceValue, "log");
        const rawText = typeof textValue === "string" ? textValue : "";
        // Parse the exact private Doctor schema before generic credential
        // redaction for the same reason as Activity: safe logical names can
        // resemble raw credential fields. The strict normalizer independently
        // whitelists, bounds, and sanitizes every retained value.
        const doctorInventories = doctorInventoryRecordsFromText(rawText);
        // Sanitize the complete response field before applying the existing
        // aggregate display ceiling. Otherwise a URL/cookie terminator just
        // beyond that cutoff can leave its leading secret in
        // the stored record and therefore in every later copy path.
        const sanitized = redactedTroubleshootingText(rawText);
        const candidate = sanitized.slice(0, remaining);
        if (!candidate && !(structured && allowEmpty)) return;
        remaining -= candidate.length;
        // Timestamps are read from the redacted, display-bounded text rather
        // than from the raw response, so the time shown beside a line always
        // belongs to the line actually rendered next to it.
        const lines = candidate ? logLinesWithTime(candidate) : [];
        records.push({
          id: `${records.length}:${source}`,
          source,
          text: candidate,
          troubleshootingSource: source,
          troubleshootingText: candidate,
          lines,
          lineCount: lines.length,
          doctorInventories
        });
      };
      if (model && Array.isArray(model.logs)) {
        model.logs.forEach((entry) => {
          if (typeof entry === "string") return append("log", entry);
          if (!entry || typeof entry !== "object") return;
          const source = typeof entry.source === "string" ? entry.source : "log";
          if (Array.isArray(entry.lines)) {
            append(source, entry.lines.map((line) => typeof line === "string" ? line : "").join("\n"), true);
            return;
          }
          const timestamp = typeof entry.timestamp === "string" ? entry.timestamp : "";
          const safeTimestamp = timestamp ? troubleshootingField(timestamp, "") : "";
          const safeSource = entry.source ? troubleshootingField(source, "log") : "";
          const prefix = `${safeTimestamp ? `[${safeTimestamp}] ` : ""}${safeSource ? `[${safeSource}] ` : ""}`;
          append(source, `${prefix}${typeof entry.message === "string" ? entry.message : ""}`);
        });
      } else if (model && typeof (model.text || model.output) === "string") {
        append("log", model.text || model.output);
      }
      return records;
    },
    logsFrom(model) {
      const records = this.logRecordsFrom(model);
      return records.length
        ? records.map((record) => `[${record.source}] ${record.text}`).join("\n").slice(0, MAX_RESPONSE_BYTES)
        : "No log data yet.";
    },
    async refreshLogs() {
      if (this.disposed || this.logsLoading || this.logsPaused || document.hidden || this.route !== "activity") return;
      this.logsLoading = true;
      try {
        const lines = Math.min(1000, Math.max(1, Number(this.logLines) || 200));
        // Two independent read-only feeds. Waiting on them jointly must not let
        // one failure discard the other's payload: the package log scan is far
        // more expensive than the activity feed, so a combined wait turned a
        // healthy activity response into an empty Activity list.
        const [logs, activity] = await Promise.allSettled([
          apiGet(this.auth, "logs", { lines, source: this.logSource }),
          apiGet(this.auth, "activity", { lines })
        ]);
        if (this.disposed) return;
        if (logs.status === "fulfilled") {
          const records = this.logRecordsFrom(logs.value);
          this.logRecords = records;
          this.logOutput = records.length
            ? records.map((record) => `[${record.source}] ${record.text}`).join("\n").slice(0, MAX_RESPONSE_BYTES)
            : "No log data yet.";
        }
        if (activity.status === "fulfilled") this.activityEvents = arrayOf(activity.value.events);
        this.logState = logsFeedState(logs.status === "fulfilled", activity.status === "fulfilled", lines);
      } catch (_error) {
        if (!this.disposed) this.logState = "Logs unavailable";
      } finally {
        this.logsLoading = false;
        if (!this.disposed) this.scheduleLogs();
      }
    },
    toggleLogs() { this.logsPaused = !this.logsPaused; this.logState = this.logsPaused ? "Paused" : "Resuming"; if (!this.logsPaused) this.refreshLogs(); else window.clearTimeout(this.logTimer); },
    clearLogView() { this.logRecords = []; this.logOutput = "View cleared. The package log was not deleted."; },
    logSourceClearable(source) { return CLEARABLE_LOG_SOURCES.includes(source); },
    logClearTooltip(source) {
      if (!this.logSourceClearable(source)) {
        return "The audit log records who cleared each package log, so it is deliberately not clearable from the dashboard.";
      }
      if (this.securityPolicy.allow_operational_actions === false) {
        return "Operational actions are disabled by the current security policy.";
      }
      if (!this.canMutate) return "This DSM session cannot change package state.";
      return "Empty this bounded package log and its rotated files. The package records the clearing itself in the audit log.";
    },
    async clearLogSource(source) {
      if (!this.logSourceClearable(source)) return;
      if (scopeMutationOutcomeUnresolved(this, "profile")) return this.toast("Log clearing locked", scopeMutationGuidance(this, "profile"), true);
      if (isolatedIncidentUnresolved(this, "operations")) return this.toast("Log clearing locked", isolatedIncidentGuidance(this, "operations"), true);
      if (!this.canRunOperations || this.operationBusy || this.disposed) return;
      if (!await this.confirmAction(
        `Clear the ${source} log?`,
        "The active log and its rotated files are emptied on the NAS. Evidence already collected here is lost, and the package records this clearing in the audit log.",
        "Clear log"
      )) return;
      this.operationBusy = true;
      try {
        await apiPost(this.auth, this.csrfToken, ACTIONS.clearLogs, { source });
        if (this.disposed) return;
        this.toast("Log cleared", `The package emptied the ${source} log and its rotated files.`);
      } catch (error) {
        if (this.disposed) return;
        this.reportMutationError(error, "Log not cleared", "Log clearing outcome unknown", "The package rejected the change.");
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
      if (!this.disposed) await this.refreshLogs();
    },
    async saveNotificationPreferences(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (scopeMutationOutcomeUnresolved(this, "interface")) return this.toast("Session preference save locked", scopeMutationGuidance(this, "interface"), true);
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
          const report = this.reportMutationError(
            error,
            rejected && restored ? "Session preferences not saved" : (rejected ? "Session preference rollback incomplete" : "Session preferences stored · audit failed"),
            "Session preferences stored · audit outcome unknown",
            rejected && restored
              ? "The package rejected the audit event and the prior browser preferences were restored."
              : "The browser preferences remain stored locally, but the package audit did not complete."
          );
          if (report.unknown || report.inspection) this.pauseAutosave("interface", error);
        }
      } finally {
        if (!this.disposed) this.operationBusy = false;
      }
    },
    async saveInterfaceSettings(event) {
      if (event && event.preventDefault) event.preventDefault();
      if (scopeMutationOutcomeUnresolved(this, "interface")) return this.toast("Interface save locked", scopeMutationGuidance(this, "interface"), true);
      if (!this.canChangeInterface || this.operationBusy) return;
      this.cancelAutosave("interface");
      const candidate = this.interfaceSettingsPayload();
      const invalid = this.validateInterfacePayload(candidate);
      if (invalid) return this.toast("Interface settings not saved", invalid, true);
      this.operationBusy = true;
      let transaction = null;
      try {
        transaction = this.captureSettingsTransaction();
        if (!transaction) {
          this.pauseAutosave("interface");
          return;
        }
        const next = Object.assign({}, transaction.settings, candidate);
        if (!this.persistSettings(next)) {
          this.applySettingsState(transaction.settings);
          this.hydrateAutosave("interface", this.interfaceSettingsPayload());
          this.pauseAutosave("interface");
          this.scheduleSnapshot();
          this.scheduleLogs();
          return;
        }
        this.applySettingsState(next);
        this.scheduleSnapshot();
        this.scheduleLogs();
        await apiPost(this.auth, this.csrfToken, ACTIONS.clientEvent, { event: "interface-settings" });
        if (this.disposed) return;
        this.hydrateAutosave("interface", candidate);
        this.toast("Interface settings saved", "Theme and refresh cadence were audited and stored locally.");
      } catch (error) {
        if (!this.disposed) {
          const rejected = this.preferenceAuditWasRejected(error);
          const restored = rejected && transaction ? this.restoreSettingsTransaction(transaction) : false;
          if (rejected) {
            this.scheduleSnapshot();
            this.scheduleLogs();
          }
          this.hydrateAutosave("interface", this.interfaceSettingsPayload());
          this.pauseAutosave("interface", error);
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
    pathBrowserFocusables() {
      const dialog = this.confirmationElement("pathBrowserDialog");
      if (!dialog || !dialog.querySelectorAll) return [];
      return Array.from(dialog.querySelectorAll("button, [href], input, select, textarea, [tabindex]"))
        .filter((element) => !element.disabled && element.getAttribute("tabindex") !== "-1" && element.getAttribute("aria-hidden") !== "true");
    },
    handlePathBrowserKeydown(event) {
      if (!this.pathBrowser.visible) return;
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        this.closePathBrowser();
        return;
      }
      if (event.key !== "Tab") return;
      const dialog = this.confirmationElement("pathBrowserDialog");
      const focusable = this.pathBrowserFocusables();
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
    removePathBrowserKeyHandler() {
      if (this.pathBrowserKeyHandler) {
        if (typeof document !== "undefined") document.removeEventListener("keydown", this.pathBrowserKeyHandler, true);
        this.pathBrowserKeyHandler = null;
      }
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
