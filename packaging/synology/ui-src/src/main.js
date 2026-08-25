import Vue from "vue";
import App from "./App.vue";
import "./styles/native.css";

/* global SYNO */
SYNO.namespace("SYNO.SDS.App.SynologyDriveSync");

SYNO.SDS.App.SynologyDriveSync.Instance = Vue.extend({
  components: { App },
  template: "<App/>"
});
