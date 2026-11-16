<div class="hero">
  <div class="hero-brand">
    <img class="hero-logo" src="theme/logo.svg" alt="" width="44" height="44">
    <p class="hero-kicker">synology-drive-sync</p>
  </div>
  <h1>Push files to your Synology NAS</h1>
  <p class="hero-tagline">A fast, one-way file deposit for machines that can't run Synology Drive Client.</p>
  <p class="hero-summary">
    <code>synology-drive-sync</code> pushes files from any machine to a Synology NAS over DSM's
    File Station API, using a normal DSM account (QuickConnect addresses work too). It is built for
    servers, containers, CI runners and other systems where Synology Drive Client is unsupported or
    not permitted. Use it from scripts and pipelines through the CLI, the Rust SDK or the C ABI, or
    install the included DSM package for a dashboard. It deposits files one way and does not replace
    Drive's two-way sync. This is an independent project, not affiliated with or endorsed by Synology.
  </p>
  <p class="hero-links">
    <a class="hero-link hero-link-primary" href="getting-started/quick-start.html">Quick start</a>
    <a class="hero-link" href="installation.html">Install</a>
    <a class="hero-link" href="synology-package.html">DSM package</a>
    <a class="hero-link" href="reference/cli.html">CLI reference</a>
    <a class="hero-link" href="operations/troubleshooting.html">Troubleshooting</a>
  </p>
</div>

<div class="contract-strip">
  <div><strong>Source</strong><span>Local and never modified</span></div>
  <div><strong>Destination</strong><span>Logical File Station path</span></div>
  <div><strong>Deletion</strong><span>Off unless explicitly armed</span></div>
</div>

> [!IMPORTANT]
> Automated tests do not log in to a live NAS. Before trusting production data, complete the
> [disposable live-NAS acceptance and recovery runbook](production-acceptance.md) against the exact
> NAS, DSM version, reverse proxy, account, and scheduler identity you will use.

## Choose a path

<div class="doc-grid">
  <a class="doc-card" href="getting-started/quick-start.html">
    <strong>Install and run</strong>
    <span>Verify a release, configure a profile, diagnose both ends, and review the first plan.</span>
  </a>
  <a class="doc-card" href="configuration/index.html">
    <strong>Configure everything</strong>
    <span>Understand TOML, CLI and environment precedence, secrets, TLS, logging, and deletion.</span>
  </a>
  <a class="doc-card" href="operations/scheduling.html">
    <strong>Operate unattended</strong>
    <span>Deploy finite jobs with systemd, cron, launchd, Task Scheduler, Docker, or DSM.</span>
  </a>
  <a class="doc-card" href="sdk/index.html">
    <strong>Integrate in code</strong>
    <span>Use the high-level Rust engine or the versioned C ABI from a matching DLL or shared object.</span>
  </a>
</div>

## Search without leaving the page

The search icon indexes every chapter in this book in the browser. Press `/` or `S` anywhere in the
docs and start typing a command, configuration key, environment variable, exit code, or error clue.
No hosted search account is required.

## The short safety model

- The source is authoritative and is never changed by this tool.
- Remote-only entries are preserved unless deletion is enabled at every required layer.
- Passwords, TOTP seeds, current OTP codes, and bearer-token values are not valid TOML values.
- HTTPS is required by default; private certificate authorities have an explicit trust path.
- A plan is not a backup. Enable and test independent version history, snapshots, or backup first.

Continue with [what the tool does and what it protects](getting-started/overview.md), or go directly
to the [quick start](getting-started/quick-start.md).
