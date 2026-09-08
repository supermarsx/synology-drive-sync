import Vue from "vue";
import App from "./App.vue";
import "./styles/native.css";
import runtimeCss from "./styles/native.css?runtime";
import { installRuntimeStyles } from "./runtimeStyles";
import { installWidget } from "./widget";

installRuntimeStyles(runtimeCss);

/* global SYNO */
SYNO.namespace("SYNO.SDS.App.SynologyDriveSync");

SYNO.SDS.App.SynologyDriveSync.Instance = Vue.extend({
  components: { App },
  template: "<App/>"
});

// DSM loads this same bundle for the AppWindow and for the desktop widget, and
// resolves the widget class by name the moment the script finishes executing.
// Registration must therefore be a load-time side effect, not something the
// AppWindow does when it opens.
installWidget();
