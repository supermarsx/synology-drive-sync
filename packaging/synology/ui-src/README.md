# Native DSM UI source

This directory follows Synology's documented DSM AppWindow project layout. The
runtime entry registers `SYNO.SDS.App.SynologyDriveSync.Instance` with
`Vue.extend`; `App.vue` renders the dashboard directly inside
`v-app-instance`/`v-app-window`. It does not embed the legacy web dashboard.

## Reproducible host build

The dependency pins match Synology's DSM 7.2.2 `ExamplePackage/ui` toolchain,
including Vue 2.7.14 and pnpm 8.15.9's lockfile v6 format:

```sh
pnpm install --frozen-lockfile
pnpm build
```

The architecture-neutral outputs are:

- `dist/SynologyDriveSync.js`
- `dist/style.css`

Webpack externalizes `vue` because DSM supplies the Vue runtime and its
globally registered `v-*` components. Building the two static assets therefore
needs only the public npm dependencies in the lockfile; running the UI requires
DSM.

## Toolkit and repository packaging paths

Synology's toolkit consumes `app.config`, `config.define`, and `Makefile`, then
uses `GenerateJSDepend.php` through `Makefile.js.inc` to produce an installed
configuration grouped by destination JavaScript file. The equivalent installed
shape is:

```json
{
  "SynologyDriveSync.js": {
    "SYNO.SDS.App.SynologyDriveSync.Instance": {
      "type": "app",
      "appWindow": "SYNO.SDS.App.SynologyDriveSync.Instance"
    }
  }
}
```

For reproducible GitHub-hosted builds, `../build_spk.py` performs that small
toolkit-only transformation itself and stages:

| Source | Installed SPK path |
| --- | --- |
| generated wrapper from `app.config` + `config.define` | `ui/config` |
| `../package/ui/index.html` | `ui/index.html` |
| `dist/SynologyDriveSync.js` | `ui/SynologyDriveSync.js` |
| `dist/style.css` | `ui/style.css` |
| `../package/ui/images` plus deterministic PNG renders | `ui/images` |
| `../package/ui/texts` | `ui/texts` |

`ui/index.html` is not a second dashboard. It is a script-free, no-external-asset
same-origin launcher for DSM's directory-index and explicit `index.html` routes.
It immediately opens
`/webman/index.cgi?launchApp=SYNO.SDS.App.SynologyDriveSync.Instance` and retains
an accessible fallback link to that same native AppWindow entry. The installed
`ui/config` module remains the only application implementation.

This host path does not need a Synology toolkit installation or a DSM host. The
official Makefile remains available for SDK builds where `/env.mak`, `snpm`, and
`Makefile.js.inc` are provided by Synology's toolkit.

Official references:

- <https://help.synology.com/developer-guide/synology_package/package_tgz/launch_app.html>
- <https://help.synology.com/developer-guide/appendix/ui_framework/application.html>
- <https://github.com/SynologyOpenSource/ExamplePackages/tree/main/ExamplePackage/ui>
