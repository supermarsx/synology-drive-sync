# synology-drive-sync installer for Windows (Windows PowerShell 5.1 and PowerShell 7).
#
#   irm https://raw.githubusercontent.com/supermarsx/synology-drive-sync/main/scripts/install.ps1 | iex
#
# Installs the native CLI for your CPU into %LOCALAPPDATA%\Programs\synology-drive-sync (no
# administrator rights) and adds that directory to your USER PATH only. It downloads the release
# archive and SHA256SUMS over HTTPS from github.com, checks the archive against SHA256SUMS, checks
# the binary reports the release version, and only then replaces the installed file. Any failed
# check throws with a message; nothing unverified is ever installed. It contacts only github.com
# (and the objects.githubusercontent.com host GitHub redirects downloads to) and sends no telemetry.
#
# Settings (environment variables work with `irm | iex`; parameters work when you save the script):
#   SDSYNC_VERSION / -Version        release to install, e.g. 26.66 (default: the latest release)
#   SDSYNC_INSTALL_DIR / -InstallDir directory to install into
#   SDSYNC_NO_MODIFY_PATH=1 / -NoModifyPath   leave the user PATH alone
#
# The DSM package (.spk) is not installed by this script; see the project documentation.
#
# Everything runs inside Install-SynologyDriveSync, invoked on the last line, so a download cut
# short can never execute a partial command.

function Install-SynologyDriveSync {
    # An interactive installer talks to the person running it; Write-Host is the right channel.
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '')]
    [CmdletBinding()]
    param(
        [string] $Version = $env:SDSYNC_VERSION,
        [string] $InstallDir = $env:SDSYNC_INSTALL_DIR,
        [switch] $NoModifyPath
    )

    $ErrorActionPreference = 'Stop'
    $repository = 'supermarsx/synology-drive-sync'
    $name = 'synology-drive-sync'

    if ($env:OS -ne 'Windows_NT') {
        throw 'install.ps1 is for Windows. On Linux and macOS use scripts/install.sh; the DSM package installs from Package Center.'
    }
    if (-not $InstallDir) {
        if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is not set; set SDSYNC_INSTALL_DIR.' }
        $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\synology-drive-sync'
    }
    if ($env:SDSYNC_NO_MODIFY_PATH -eq '1') { $NoModifyPath = $true }

    # Release downloads come from GitHub over HTTPS. The one exception is a test fixture:
    # SDSYNC_RELEASE_BASE_URL, honoured only when it names a local file:// directory or a loopback
    # 127.0.0.1 server, so a hostile environment variable cannot redirect a real install elsewhere.
    # Layout of the fixture: <base>/latest-version and <base>/<version>/<asset>.
    $fixture = $null
    if ($env:SDSYNC_RELEASE_BASE_URL) {
        $candidate = $env:SDSYNC_RELEASE_BASE_URL.TrimEnd('/')
        if ($candidate -match '^file:///' -or $candidate -match '^http://127\.0\.0\.1(:\d+)?(/|$)') {
            $fixture = $candidate
        }
        else {
            throw "SDSYNC_RELEASE_BASE_URL is only honoured for file:// or http://127.0.0.1 URLs (got: $candidate)"
        }
    }

    # TLS 1.2 for Windows PowerShell 5.1; harmless on PowerShell 7.
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    }
    catch { Write-Verbose "Could not force TLS 1.2: $($_.Exception.Message)" }

    function Get-Remote([string] $Url, [string] $Destination) {
        if ($Url -match '^file:///') {
            $path = ([Uri] $Url).LocalPath
            if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { return $false }
            Copy-Item -LiteralPath $path -Destination $Destination
            return $true
        }
        if ($Url -notmatch '^https://' -and $Url -notmatch '^http://127\.0\.0\.1') {
            throw "Refusing to download from a non-HTTPS URL: $Url"
        }
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Destination | Out-Null
            return $true
        }
        catch {
            return $false
        }
    }

    $archKey = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    switch ($archKey) {
        'AMD64' { $arch = 'x86_64' }
        'ARM64' { $arch = 'aarch64' }
        default { throw "Unsupported CPU architecture: $archKey. Releases ship x86_64 and aarch64 builds for Windows." }
    }

    $work = Join-Path ([IO.Path]::GetTempPath()) ('sdsync-install-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $work | Out-Null
    try {
        if (-not $Version) {
            if ($fixture) {
                $latestFile = Join-Path $work 'latest-version'
                if (-not (Get-Remote "$fixture/latest-version" $latestFile)) { throw "Could not read $fixture/latest-version" }
                $Version = (Get-Content -LiteralPath $latestFile -TotalCount 1).Trim()
            }
            else {
                # github.com/OWNER/REPO/releases/latest redirects to .../releases/tag/<version>.
                $request = [Net.HttpWebRequest]::Create("https://github.com/$repository/releases/latest")
                $request.Method = 'HEAD'
                $request.AllowAutoRedirect = $false
                $request.UserAgent = 'synology-drive-sync-installer'
                try {
                    $response = $request.GetResponse()
                }
                catch {
                    throw "Could not look up the latest release at github.com/${repository}: $($_.Exception.Message)"
                }
                try { $location = $response.Headers['Location'] } finally { $response.Close() }
                if (-not $location) { throw "github.com did not report a latest release for $repository." }
                $Version = ($location.TrimEnd('/') -split '/')[-1]
            }
        }
        if ($Version -notmatch '^[0-9]{2}\.[1-9][0-9]*$') {
            throw "Release version must look like 26.66 (got: $Version)"
        }

        $asset = "$name-$Version-windows-$arch.zip"
        $releaseUrl = if ($fixture) { "$fixture/$Version" } else { "https://github.com/$repository/releases/download/$Version" }

        Write-Host "Installing $name $Version for windows-$arch"
        $archive = Join-Path $work $asset
        $sums = Join-Path $work 'SHA256SUMS'
        if (-not (Get-Remote "$releaseUrl/$asset" $archive)) {
            throw "Release $Version has no $asset (is $Version a published release for windows-$arch?)"
        }
        if (-not (Get-Remote "$releaseUrl/SHA256SUMS" $sums)) {
            throw "Release $Version has no SHA256SUMS; refusing to install an unverified archive."
        }

        $pattern = '^([0-9A-Fa-f]{64})\s+\*?' + [Regex]::Escape($asset) + '\s*$'
        $listed = @(Get-Content -LiteralPath $sums | Where-Object { $_ -match $pattern })
        if ($listed.Count -ne 1) {
            throw "SHA256SUMS must list exactly one checksum for $asset; refusing to install."
        }
        $expected = ([regex]::Match($listed[0], $pattern)).Groups[1].Value.ToLowerInvariant()
        $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $expected) {
            throw "SHA-256 mismatch for $asset (expected $expected, got $actual); nothing was installed."
        }
        Write-Host 'Checksum verified against SHA256SUMS'

        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $member = "$name-$Version-windows-$arch/$name.exe"
        $candidate = Join-Path $work "$name.exe"
        $zip = [IO.Compression.ZipFile]::OpenRead($archive)
        try {
            $matching = @()
            foreach ($entry in $zip.Entries) {
                $entryName = $entry.FullName
                if ($entryName.StartsWith('/') -or $entryName.StartsWith('\') -or $entryName -match '^[A-Za-z]:' -or $entryName.Contains('\') -or $entryName -match '(^|/)\.\.(/|$)') {
                    throw "The archive contains an unsafe path: $entryName"
                }
                if ($entryName -ceq $member) { $matching += $entry }
            }
            if ($matching.Count -ne 1) { throw "The archive does not contain exactly one $member" }
            $unixType = (($matching[0].ExternalAttributes -shr 16) -band 0xF000)
            if ($unixType -ne 0 -and $unixType -ne 0x8000) { throw "$member in the archive is not a regular file" }
            [IO.Compression.ZipFileExtensions]::ExtractToFile($matching[0], $candidate, $true)
        }
        finally {
            $zip.Dispose()
        }

        $reported = (& $candidate --version 2>&1 | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) { throw "The downloaded binary did not run: $reported" }
        if ($reported -cne "$name $Version") {
            throw "The downloaded binary reports '$reported', expected '$name $Version'; nothing was installed."
        }

        if (Test-Path -LiteralPath $InstallDir) {
            $dirItem = Get-Item -LiteralPath $InstallDir -Force
            if (-not $dirItem.PSIsContainer) { throw "$InstallDir exists and is not a directory." }
        }
        else {
            New-Item -ItemType Directory -Path $InstallDir | Out-Null
        }
        $InstallDir = (Resolve-Path -LiteralPath $InstallDir).ProviderPath
        $target = Join-Path $InstallDir "$name.exe"

        $previous = $null
        if (Test-Path -LiteralPath $target) {
            $targetItem = Get-Item -LiteralPath $target -Force
            if ($targetItem.PSIsContainer -or ($targetItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                throw "$target exists and is not a regular file; remove it first."
            }
            try { $previous = (& $target --version 2>&1 | Out-String).Trim() } catch { $previous = $null }
        }

        # Stage next to the target and swap, so a re-run replaces it in one step.
        $staged = Join-Path $InstallDir ".$name.install.$([Guid]::NewGuid().ToString('N'))"
        try {
            Copy-Item -LiteralPath $candidate -Destination $staged
            try {
                if (Test-Path -LiteralPath $target) {
                    # The swap keeps the old file as a backup until it succeeds, then drops it.
                    $backup = "$staged.old"
                    [IO.File]::Replace($staged, $target, $backup)
                    Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
                }
                else {
                    [IO.File]::Move($staged, $target)
                }
            }
            catch {
                throw "Could not replace $target (is synology-drive-sync running?): $($_.Exception.Message)"
            }
        }
        finally {
            if (Test-Path -LiteralPath $staged) { Remove-Item -LiteralPath $staged -Force }
        }

        $installed = (& $target --version 2>&1 | Out-String).Trim()
        if ($LASTEXITCODE -ne 0 -or $installed -cne "$name $Version") {
            throw "The installed binary reports '$installed', expected '$name $Version'."
        }

        if (-not $previous) { Write-Host "Installed $installed to $target" }
        elseif ($previous -ceq $installed) { Write-Host "Reinstalled $installed at $target (already at this version)" }
        else { Write-Host "Upgraded $previous -> $installed at $target" }

        $trimmed = $InstallDir.TrimEnd('\')
        $pathEntries = @($env:Path -split ';' | Where-Object { $_ } | ForEach-Object { $_.TrimEnd('\') })
        if ($NoModifyPath) {
            if ($pathEntries -notcontains $trimmed) {
                Write-Host ''
                Write-Host "$InstallDir is not on your PATH (left alone, as requested)."
            }
        }
        else {
            $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
            $userEntries = @($userPath -split ';' | Where-Object { $_ })
            if (@($userEntries | ForEach-Object { $_.TrimEnd('\') }) -notcontains $trimmed) {
                [Environment]::SetEnvironmentVariable('Path', ((@($userEntries) + $InstallDir) -join ';'), 'User')
                Write-Host ''
                Write-Host "Added $InstallDir to your user PATH (not the machine PATH). Open a new terminal to use it."
            }
            if ($pathEntries -notcontains $trimmed) { $env:Path = "$env:Path;$InstallDir" }
        }

        Write-Host ''
        Write-Host "Uninstall: Remove-Item '$target'  (profiles, logs and saved credentials are kept; remove the PATH entry in Settings > Environment Variables if you wish)"
    }
    finally {
        if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue }
    }
}

Install-SynologyDriveSync @args
