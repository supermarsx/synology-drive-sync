import Vue from "vue";
import App from "./App.vue";
import "./styles/native.css";
import runtimeCss from "./styles/native.css?runtime";
import { installRuntimeStyles } from "./runtimeStyles";
import { installWidget } from "./widget";
import { reportUiError } from "./uiErrors.mjs";

installRuntimeStyles(runtimeCss);

/* global SYNO */
SYNO.namespace("SYNO.SDS.App.SynologyDriveSync");

// Vue is DSM's global (see the externals entry in webpack.config.js), so a
// Vue.config.errorHandler would take over every other DSM app's errors too.
// Errors raised anywhere under <App/> -- including App's own render, watchers
// and handlers, whose parent is this wrapper -- reach errorCaptured here, where
// they become a banner in the window instead of a line in the console and a
// page that has silently stopped updating. Returning nothing lets the error
// continue to Vue's default handler, so DSM's own console logging is unchanged.
SYNO.SDS.App.SynologyDriveSync.Instance = Vue.extend({
  components: { App },
  template: "<App/>",
  errorCaptured(error, vm, info) {
    reportUiError(error, info, this);
  }
});

// DSM loads this same bundle for the AppWindow and for the desktop widget, and
// resolves the widget class by name the moment the script finishes executing.
// Registration must therefore be a load-time side effect, not something the
// AppWindow does when it opens.
installWidget();
