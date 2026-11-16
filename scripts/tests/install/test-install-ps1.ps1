# Tests scripts/install.ps1 against a local fake release. No network, no browser, no UI.
#
#   powershell -NoProfile -File test-install-ps1.ps1 [-Shell powershell|pwsh] [-Installer PATH]
#
# The fake "binary" is a tiny console program compiled with Windows PowerShell 5.1's Add-Type, so
# the release fixture is built by powershell.exe even when -Shell pwsh runs the installer under test.
# The user PATH is only written when SDSYNC_TEST_ALLOW_PATH_WRITE=1 (CI runners); on a developer
# machine that one check is skipped so the test never edits your environment.
param(
    [string] $Shell = 'powershell',
    [string] $Installer = (Join-Path $PSScriptRoot '..\..\install.ps1')
)

$ErrorActionPreference = 'Stop'
$Installer = (Resolve-Path -LiteralPath $Installer).ProviderPath
$root = Join-Path ([IO.Path]::GetTempPath()) ('sdsync-install-test-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
$release = Join-Path $root 'release'
New-Item -ItemType Directory -Path $release | Out-Null

# ---- fixture -------------------------------------------------------------------------------

function New-FakeExe([string] $Reported, [string] $Path) {
    $source = @"
public static class Program { public static int Main(string[] a) {
  if (a.Length > 0 && a[0] == "--version") { System.Console.WriteLine("synology-drive-sync $Reported"); return 0; }
  System.Console.WriteLine("fake synology-drive-sync $Reported"); return 0; } }
"@
    $helper = Join-Path $root "compile-$Reported.ps1"
    @"
Add-Type -TypeDefinition @'
$source
'@ -OutputAssembly '$Path' -OutputType ConsoleApplication
"@ | Set-Content -LiteralPath $helper -Encoding UTF8
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $helper
    if (-not (Test-Path -LiteralPath $Path)) { throw "could not compile the fake binary for $Reported" }
}

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

function New-FakeRelease([string] $Version, [string] $Reported) {
    $exe = Join-Path $root "fake-$Reported.exe"
    if (-not (Test-Path -LiteralPath $exe)) { New-FakeExe $Reported $exe }
    $dir = Join-Path $release $Version
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    $lines = @()
    foreach ($arch in @('x86_64', 'aarch64')) {
        $rootName = "synology-drive-sync-$Version-windows-$arch"
        $zipPath = Join-Path $dir "$rootName.zip"
        $zip = [IO.Compression.ZipFile]::Open($zipPath, 'Create')
        try {
            [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $exe, "$rootName/synology-drive-sync.exe")
        }
        finally { $zip.Dispose() }
        $hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
        $lines += "$hash  $rootName.zip"
    }
    Set-Content -LiteralPath (Join-Path $dir 'SHA256SUMS') -Value $lines -Encoding ASCII
}

New-FakeRelease '26.64' '26.64'
New-FakeRelease '26.65' '26.65'
Set-Content -LiteralPath (Join-Path $release 'latest-version') -Value '26.65' -Encoding ASCII
New-FakeRelease '26.63' '26.63'                    # tampered below
foreach ($zip in Get-ChildItem -LiteralPath (Join-Path $release '26.63') -Filter '*.zip') {
    [IO.File]::AppendAllText($zip.FullName, 'tampered')
}
New-FakeRelease '26.62' '26.62'                    # listed but never uploaded
Get-ChildItem -LiteralPath (Join-Path $release '26.62') -Filter '*.zip' | Remove-Item
New-FakeRelease '26.61' '26.61'                    # no SHA256SUMS
Remove-Item -LiteralPath (Join-Path $release '26.61\SHA256SUMS')
New-FakeRelease '26.60' '26.59'                    # binary reports another version

# ---- harness -------------------------------------------------------------------------------

$script:passed = 0
$script:failed = 0
function Pass([string] $Name) { $script:passed++; Write-Host "ok   $Name" }
function Fail([string] $Name, [string] $Detail) { $script:failed++; Write-Host "FAIL $Name"; if ($Detail) { Write-Host ($Detail -replace '(?m)^', '       | ') } }

$releaseUri = ([Uri] $release).AbsoluteUri
$dest = Join-Path $root 'bin'
$script:output = ''
$script:exitCode = 0

function Invoke-Installer([hashtable] $Environment = @{}, [string[]] $Arguments = @(), [switch] $ViaIex) {
    $saved = @{}
    $settings = @{
        SDSYNC_RELEASE_BASE_URL = $releaseUri
        SDSYNC_INSTALL_DIR      = $dest
        SDSYNC_NO_MODIFY_PATH   = '1'
        SDSYNC_VERSION          = $null
    }
    foreach ($key in $Environment.Keys) { $settings[$key] = $Environment[$key] }
    foreach ($key in $settings.Keys) {
        $saved[$key] = [Environment]::GetEnvironmentVariable($key, 'Process')
        [Environment]::SetEnvironmentVariable($key, $settings[$key], 'Process')
    }
    try {
        $exe = if ($Shell -eq 'pwsh') { 'pwsh' } else { 'powershell.exe' }
        $previousPreference = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        if ($ViaIex) {
            # What `irm <url> | iex` does: the script text is evaluated, with settings from the environment.
            $command = "Get-Content -LiteralPath '$Installer' -Raw | Invoke-Expression"
            $script:output = (& $exe -NoProfile -ExecutionPolicy Bypass -Command $command 2>&1 | Out-String)
        }
        else {
            $script:output = (& $exe -NoProfile -ExecutionPolicy Bypass -File $Installer @Arguments 2>&1 | Out-String)
        }
        $script:exitCode = $LASTEXITCODE
        $ErrorActionPreference = $previousPreference
    }
    finally {
        foreach ($key in $saved.Keys) { [Environment]::SetEnvironmentVariable($key, $saved[$key], 'Process') }
    }
}
function Installed-Version { (& (Join-Path $dest 'synology-drive-sync.exe') --version | Out-String).Trim() }
function Has([string] $Text) { $script:output.Contains($Text) }

# 1. happy path
Invoke-Installer
if ($exitCode -eq 0 -and (Installed-Version) -ceq 'synology-drive-sync 26.65' -and (Has 'Checksum verified') -and (Has 'Installed synology-drive-sync 26.65') -and (Has 'Uninstall:')) {
    Pass 'latest release installs and reports its version'
} else { Fail 'latest release installs and reports its version' $output }

# 2. idempotent re-run
Invoke-Installer
if ($exitCode -eq 0 -and (Has 'Reinstalled synology-drive-sync 26.65')) { Pass 're-run of the same version is idempotent' }
else { Fail 're-run of the same version is idempotent' $output }

# 3. pinned older version then upgrade
Invoke-Installer @{ SDSYNC_VERSION = '26.64' }
if ($exitCode -eq 0 -and (Has 'Upgraded synology-drive-sync 26.65 -> synology-drive-sync 26.64')) { Pass 'pinning SDSYNC_VERSION installs that release' }
else { Fail 'pinning SDSYNC_VERSION installs that release' $output }
Invoke-Installer
if ($exitCode -eq 0 -and (Has 'Upgraded synology-drive-sync 26.64 -> synology-drive-sync 26.65') -and (Installed-Version) -ceq 'synology-drive-sync 26.65') { Pass 'upgrade replaces the installed file' }
else { Fail 'upgrade replaces the installed file' $output }
if (@(Get-ChildItem -LiteralPath $dest -Force).Count -eq 1) { Pass 'no staging files left behind' }
else { Fail 'no staging files left behind' ((Get-ChildItem -LiteralPath $dest -Force | Out-String)) }

# 4. checksum mismatch keeps the old binary
Invoke-Installer @{ SDSYNC_VERSION = '26.63' }
if ($exitCode -ne 0 -and (Has 'SHA-256 mismatch') -and (Installed-Version) -ceq 'synology-drive-sync 26.65') { Pass 'checksum mismatch refuses to install and keeps the old binary' }
else { Fail 'checksum mismatch refuses to install and keeps the old binary' $output }

# 5. missing asset, missing sums, wrong reported version, unknown release, bad version
Invoke-Installer @{ SDSYNC_VERSION = '26.62' }
if ($exitCode -ne 0 -and (Has 'has no synology-drive-sync-26.62-')) { Pass 'missing asset fails with a clear message' } else { Fail 'missing asset fails with a clear message' $output }
Invoke-Installer @{ SDSYNC_VERSION = '26.61' }
if ($exitCode -ne 0 -and (Has 'no SHA256SUMS')) { Pass 'missing SHA256SUMS refuses to install' } else { Fail 'missing SHA256SUMS refuses to install' $output }
Invoke-Installer @{ SDSYNC_VERSION = '26.60' }
if ($exitCode -ne 0 -and (Has "expected 'synology-drive-sync 26.60'")) { Pass 'binary reporting the wrong version is refused' } else { Fail 'binary reporting the wrong version is refused' $output }
Invoke-Installer @{ SDSYNC_VERSION = '26.99' }
if ($exitCode -ne 0 -and (Has 'has no synology-drive-sync-26.99-')) { Pass 'unknown release fails with a clear message' } else { Fail 'unknown release fails with a clear message' $output }
Invoke-Installer @{ SDSYNC_VERSION = 'latest' }
if ($exitCode -ne 0 -and (Has 'Release version must look like')) { Pass 'malformed version is refused' } else { Fail 'malformed version is refused' $output }

# 6. unsupported architecture
Invoke-Installer @{ PROCESSOR_ARCHITECTURE = 'x86' }
if ($exitCode -ne 0 -and (Has 'Unsupported CPU architecture: x86')) { Pass 'unsupported architecture is refused' } else { Fail 'unsupported architecture is refused' $output }

# 7. a non-local base URL is refused, not ignored
Invoke-Installer @{ SDSYNC_RELEASE_BASE_URL = 'https://example.invalid/release' }
if ($exitCode -ne 0 -and (Has 'only honoured for file:// or http://127.0.0.1')) { Pass 'a non-local SDSYNC_RELEASE_BASE_URL is refused' } else { Fail 'a non-local SDSYNC_RELEASE_BASE_URL is refused' $output }

# 8. parameters work like the environment variables
$paramDest = Join-Path $root 'parambin'
Invoke-Installer @{ SDSYNC_INSTALL_DIR = $null } @('-Version', '26.64', '-InstallDir', $paramDest, '-NoModifyPath')
if ($exitCode -eq 0 -and ((& (Join-Path $paramDest 'synology-drive-sync.exe') --version | Out-String).Trim()) -ceq 'synology-drive-sync 26.64') { Pass '-Version and -InstallDir work' }
else { Fail '-Version and -InstallDir work' $output }

# 8b. the `irm | iex` form, configured through the environment
$iexDest = Join-Path $root 'iexbin'
Invoke-Installer @{ SDSYNC_INSTALL_DIR = $iexDest; SDSYNC_VERSION = '26.64' } -ViaIex
if ($exitCode -eq 0 -and ((& (Join-Path $iexDest 'synology-drive-sync.exe') --version | Out-String).Trim()) -ceq 'synology-drive-sync 26.64') { Pass 'works when piped to iex (irm | iex)' }
else { Fail 'works when piped to iex (irm | iex)' $output }

# 9. user PATH (only on disposable runners)
if ($env:SDSYNC_TEST_ALLOW_PATH_WRITE -eq '1') {
    $pathDest = Join-Path $root 'pathbin'
    $before = [Environment]::GetEnvironmentVariable('Path', 'User')
    $machineBefore = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    try {
        Invoke-Installer @{ SDSYNC_INSTALL_DIR = $pathDest; SDSYNC_NO_MODIFY_PATH = $null }
        $after = [Environment]::GetEnvironmentVariable('Path', 'User')
        $machineAfter = [Environment]::GetEnvironmentVariable('Path', 'Machine')
        if ($exitCode -eq 0 -and ($after -split ';') -contains (Resolve-Path -LiteralPath $pathDest).ProviderPath -and $machineBefore -ceq $machineAfter) { Pass 'the user PATH (not the machine PATH) gains the install directory' }
        else { Fail 'the user PATH (not the machine PATH) gains the install directory' $output }
        Invoke-Installer @{ SDSYNC_INSTALL_DIR = $pathDest; SDSYNC_NO_MODIFY_PATH = $null }
        $again = [Environment]::GetEnvironmentVariable('Path', 'User')
        if ($again -ceq $after) { Pass 'a re-run does not duplicate the PATH entry' } else { Fail 'a re-run does not duplicate the PATH entry' $again }
    }
    finally { [Environment]::SetEnvironmentVariable('Path', $before, 'User') }
}
else {
    Write-Host 'skip the user PATH check (set SDSYNC_TEST_ALLOW_PATH_WRITE=1 on a disposable machine)'
}

Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
Write-Host ''
Write-Host "$passed passed, $failed failed"
if ($failed -ne 0) { exit 1 }
