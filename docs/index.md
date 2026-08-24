<div class="hero">
  <p class="hero-kicker">One-way File Station synchronization</p>
  <h1>Move local folders with an explicit safety contract.</h1>
  <p class="hero-summary">
    <code>synology-drive-sync</code> plans and pushes a local directory into a chosen Synology
    File Station path through one reverse-proxy URL. These docs cover installation, every
    configuration layer, unattended operation, library integration, and release verification.
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
