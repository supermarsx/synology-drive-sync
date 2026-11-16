/* Stand-ins for the parts of DSM the dashboard bundle expects to find, for the README screenshot.
 *
 * The AppWindow bundle externalises Vue and DSM's `v-*` form components: DSM supplies them at
 * runtime. This file supplies just enough of each to let the real bundle (packaging/synology/ui-src/dist)
 * render outside DSM. It draws a plain window frame, buttons and fields; it does not imitate DSM's
 * look, and it contains none of the dashboard's own markup, text or styling. */
(function () {
  var Vue = window.Vue;

  window.SYNO = {
    namespace: function (path) {
      var node = window;
      path.split(".").forEach(function (part) { node[part] = node[part] || {}; node = node[part]; });
      return node;
    }
  };
  window.SYNO.SDS = window.SYNO.SDS || {};

  // The application instance wrapper: renders its default slot.
  Vue.component("v-app-instance", { render: function (h) { return h("div", { class: "demo-app-instance" }, this.$slots.default); } });

  // The window frame.
  Vue.component("v-app-window", {
    props: { title: String },
    render: function (h) {
      return h("div", { class: "demo-window" }, [
        h("div", { class: "demo-window-bar" }, [
          h("span", { class: "demo-window-title" }, this.title),
          h("span", { class: "demo-window-controls", attrs: { "aria-hidden": "true" } }, [h("i"), h("i"), h("i")])
        ]),
        h("div", { class: "demo-window-body" }, this.$slots.default)
      ]);
    }
  });

  Vue.component("v-button", {
    props: { type: String, suffix: String, display: String, tooltip: String, disabled: Boolean },
    render: function (h) {
      var self = this;
      var kids = [];
      if (this.$slots.icon) kids.push(h("span", { class: "demo-button-icon" }, this.$slots.icon));
      if (this.$slots.default) kids.push(h("span", { class: "demo-button-text" }, this.$slots.default));
      return h("button", {
        class: ["v-button", "demo-button", this.type ? "type-" + this.type : "", this.suffix ? "suffix-" + this.suffix : ""],
        attrs: { type: "button", disabled: this.disabled || null, title: this.tooltip || null },
        on: { click: function (event) { self.$emit("click", event); } }
      }, kids);
    }
  });

  Vue.component("v-checkbox", {
    props: { value: Boolean, disabled: Boolean },
    model: { prop: "value", event: "input" },
    render: function (h) {
      var self = this;
      return h("input", { class: "v-checkbox-input", attrs: { type: "checkbox", disabled: this.disabled || null }, domProps: { checked: this.value },
        on: { change: function (event) { self.$emit("input", event.target.checked); } } });
    }
  });

  Vue.component("v-form", { render: function (h) { return h("form", { class: "demo-form" }, this.$slots.default); } });
  Vue.component("v-form-item", {
    props: { label: String },
    render: function (h) {
      return h("div", { class: "v-form-item" }, [h("div", { class: "v-form-item-label" }, [h("label", this.label), this.$slots["label-after"]]), h("div", { class: "v-form-item-input" }, this.$slots.default)]);
    }
  });
  Vue.component("v-input", {
    props: { value: [String, Number] },
    model: { prop: "value", event: "input" },
    render: function (h) {
      var self = this;
      return h("div", { class: "v-textfield" }, [h("input", { class: "v-textfield-input-element", domProps: { value: this.value },
        on: { input: function (event) { self.$emit("input", event.target.value); } } })]);
    }
  });
  Vue.component("v-single-select", {
    props: { value: null, options: Array },
    model: { prop: "value", event: "input" },
    render: function (h) {
      var self = this;
      return h("select", { class: "v-select2-input", domProps: { value: this.value }, on: { change: function (event) { self.$emit("input", event.target.value); } } },
        (this.options || []).map(function (option) { return h("option", { attrs: { value: option.value } }, option.text || option.label || option.value); }));
    }
  });
})();
