import Vue from "vue";
import WidgetPanel from "./WidgetPanel.vue";

/* global Ext, SYNO */

/**
 * DSM desktop widget registration.
 *
 * DSM builds SYNO.SDS.Config.FnMap from the installed `ui/config` and offers
 * every entry whose `config.type` is "widget" in the desktop's widget picker
 * (see the desktop widget card list: it filters FnMap on that exact value and
 * then calls SYNO.SDS.StatusNotifier.isSupportedWidget on each class). When a
 * card is added, DSM resolves the class with Ext.getClassByName -- a plain
 * dotted lookup on the global object -- and constructs it with
 * `{ renderTo, jsConfig, appWin, height, width: 318 }`, exactly the way it
 * constructs its own SYNO.SDS.ResourceMonitor.Widget.
 *
 * So the widget must be an Ext.Panel, not a Vue class: DSM passes `renderTo`
 * and afterwards calls setHeight/doLayout/doExpand/doCollapse/onActivate/
 * onDeactivate/destroy on the object it built. This module is that adapter and
 * nothing more; it owns the Ext contract and hands every pixel and every
 * request to the Vue panel below it.
 */
// This must stay identical to the widget key in app.config: DSM looks the card
// class up by that exact dotted name.
export const WIDGET_CLASS = "SYNO.SDS.App.SynologyDriveSync.Widget";

// Built on first render rather than at bundle load, so a session that only ever
// opens the AppWindow pays nothing for the widget, and rebuilt never: a viewer
// can pin the card, remove it, and pin it again within one DSM session.
let PanelConstructor = null;

function globalExt() {
  return typeof Ext === "undefined" ? null : Ext;
}

function globalSyno() {
  return typeof SYNO === "undefined" ? null : SYNO;
}

/**
 * Define the widget panel class and publish it where Ext.getClassByName finds
 * it.
 *
 * Returns the registered class, or null when the bundle is executing without
 * the DSM desktop toolkit. That case is not an error: the same bundle is
 * loaded for the AppWindow, and a missing Ext must never keep the dashboard
 * from starting.
 */
export function installWidget() {
  const ext = globalExt();
  const syno = globalSyno();
  if (!ext || !syno || typeof ext.extend !== "function" || !ext.Panel) return null;

  const namespace = syno.SDS && syno.SDS.App && syno.SDS.App.SynologyDriveSync;
  if (!namespace) return null;
  if (namespace.Widget) return namespace.Widget;

  const base = ext.Panel.prototype;

  namespace.Widget = ext.extend(ext.Panel, {
    cls: "sdsync-widget-host",
    border: false,
    // No taskbar mini widget: DSM only builds one when the panel declares both
    // `minimizable` and a `toggleButtonCls`, and a tray button duplicating a
    // status this package already delivers through synodsmnotify would be a
    // third notification surface for the same events.
    minimizable: false,

    afterRender() {
      base.afterRender.apply(this, arguments);
      this.mountWidgetPanel();
    },

    mountWidgetPanel() {
      if (this.widgetPanel || !this.body || !this.body.dom) return;
      const host = document.createElement("div");
      this.body.dom.appendChild(host);
      if (!PanelConstructor) PanelConstructor = Vue.extend(WidgetPanel);
      this.widgetPanel = new PanelConstructor();
      this.widgetPanel.$mount(host);
      if (this.widgetActivated) this.widgetPanel.activate();
    },

    // DSM calls onActivate when the card becomes visible and onDeactivate when
    // it is minimised, the widget tray is hidden, or the card is removed. This
    // is the whole reason the widget is allowed to poll at all: it holds no
    // timer while it is not on screen.
    onActivate() {
      this.widgetActivated = true;
      if (this.widgetPanel) this.widgetPanel.activate();
    },

    onDeactivate() {
      this.widgetActivated = false;
      if (this.widgetPanel) this.widgetPanel.deactivate();
    },

    doExpand() {
      if (this.widgetPanel) this.widgetPanel.setCompact(false);
    },

    doCollapse() {
      if (this.widgetPanel) this.widgetPanel.setCompact(true);
    },

    destroy() {
      this.onDeactivate();
      if (this.widgetPanel) {
        this.widgetPanel.$destroy();
        this.widgetPanel = null;
      }
      base.destroy.apply(this, arguments);
    }
  });

  return namespace.Widget;
}
