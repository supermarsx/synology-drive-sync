const ACTION_ICON_PATHS = Object.freeze({
  overview: ["M4 4h6v6H4z", "M14 4h6v6h-6z", "M4 14h6v6H4z", "M14 14h6v6h-6z"],
  profiles: ["M3 7h7l2 2h9v10H3z", "M3 7V5h7l2 2"],
  folder: ["M3 7h7l2 2h9v10H3z", "M3 7V5h7l2 2"],
  up: ["M12 19V5", "M6 11l6-6 6 6"],
  routines: ["M12 3a9 9 0 1 0 9 9", "M12 7v5l3 2", "M17 3h4v4"],
  health: ["M3 12h4l2-5 4 10 2-5h6", "M20.8 5.7A5.5 5.5 0 0 0 12 7.1 5.5 5.5 0 0 0 3.2 5.7"],
  activity: ["M8 6h12", "M8 12h12", "M8 18h12", "M4 6h.01", "M4 12h.01", "M4 18h.01"],
  notifications: ["M18 8a6 6 0 0 0-12 0c0 7-3 7-3 9h18c0-2-3-2-3-9", "M10 21h4"],
  security: ["M12 3l8 3v5c0 5-3.4 8.5-8 10-4.6-1.5-8-5-8-10V6z", "M9 12l2 2 4-5"],
  settings: ["M4 6h10", "M18 6h2", "M4 12h3", "M11 12h9", "M4 18h7", "M15 18h5", "M14 4v4", "M7 10v4", "M11 16v4"],
  about: ["M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18", "M12 11v6", "M12 7h.01"],
  help: ["M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18", "M9.8 9a2.4 2.4 0 1 1 3.2 2.3c-.7.3-1 .8-1 1.7", "M12 17h.01"],
  refresh: ["M20 6v5h-5", "M4 18v-5h5", "M6.1 9a7 7 0 0 1 11.7-2.4L20 9", "M4 15l2.2 2.4A7 7 0 0 0 17.9 15"],
  plan: ["M8 4h8", "M9 3h6v3H9z", "M6 5H4v16h16V5h-2", "M8 12l2 2 5-5"],
  run: ["M7 4l12 8-12 8z"],
  navigate: ["M5 12h14", "M14 7l5 5-5 5"],
  add: ["M12 5v14", "M5 12h14"],
  edit: ["M4 20l4.5-1 10-10-3.5-3.5-10 10z", "M13.5 7l3.5 3.5"],
  close: ["M6 6l12 12", "M18 6L6 18"],
  delete: ["M4 7h16", "M9 7V4h6v3", "M7 7l1 14h8l1-14", "M10 11v6", "M14 11v6"],
  save: ["M5 3h12l2 2v16H5z", "M8 3v6h8V3", "M8 21v-7h8v7"],
  doctor: ["M4 5v5a4 4 0 0 0 8 0V5", "M7 3v3", "M10 3v3", "M12 10v3a4 4 0 0 0 8 0v-1", "M20 9a3 3 0 1 0 0 6"],
  pause: ["M8 5v14", "M16 5v14"],
  clear: ["M4 16l9-11 7 6-8 10H7z", "M10 19h10"],
  confirm: ["M5 12l4 4L19 6"],
  "chevron-down": ["M6 9l6 6 6-6"]
});

export const ActionIcon = {
  name: "ActionIcon",
  functional: true,
  props: {
    name: {
      type: String,
      required: true,
      validator(value) { return Object.prototype.hasOwnProperty.call(ACTION_ICON_PATHS, value); }
    },
    size: { type: [Number, String], default: 16 }
  },
  render(createElement, context) {
    return createElement("svg", {
      class: "sdsync-action-icon",
      attrs: {
        xmlns: "http://www.w3.org/2000/svg",
        width: context.props.size,
        height: context.props.size,
        viewBox: "0 0 24 24",
        fill: "none",
        stroke: "currentColor",
        "stroke-width": "1.8",
        "stroke-linecap": "square",
        "stroke-linejoin": "miter",
        "aria-hidden": "true",
        focusable: "false"
      },
      style: { display: "inline-block", verticalAlign: "-0.15em", flex: "0 0 auto" }
    }, ACTION_ICON_PATHS[context.props.name].map((path, index) => createElement("path", {
      key: index,
      attrs: { d: path }
    })));
  }
};

export default ActionIcon;
