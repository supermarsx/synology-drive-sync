[CmdletBinding(SupportsShouldProcess)]
param(
    [ValidatePattern('^[0-9]{2}\.[1-9][0-9]*$')]
    [string] $Version,

    [ValidateNotNullOrEmpty()]
    [string] $InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\synology-drive-sync'),

    [ValidatePattern('^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$')]
    [string] $Repository = 'supermarsx/synology-drive-sync',

    [switch] $AddToUserPath,

    [switch] $Uninstall
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'install.ps1 supports Windows only.'
}

if ($Uninstall) {
    foreach ($incompatible in @('Version', 'Repository', 'AddToUserPath')) {
        if ($PSBoundParameters.ContainsKey($incompatible)) {
            throw "-$incompatible cannot be combined with -Uninstall."
        }
    }
    $resolvedInstallDir = [IO.Path]::GetFullPath($InstallDir)
    if (-not (Test-Path -LiteralPath $resolvedInstallDir)) {
        Write-Host "synology-drive-sync is already absent from $resolvedInstallDir"
        return
    }
    $installItem = Get-Item -LiteralPath $resolvedInstallDir -Force
    if (-not $installItem.PSIsContainer -or
        (($installItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "Install directory is not a non-reparse directory: $resolvedInstallDir"
    }
    $target = Join-Path $resolvedInstallDir 'synology-drive-sync.exe'
    if (-not (Test-Path -LiteralPath $target)) {
        Write-Host "synology-drive-sync is already absent from $target"
        return
    }
    $targetItem = Get-Item -LiteralPath $target -Force
    if ($targetItem.PSIsContainer -or
        (($targetItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "Refusing to remove a directory or reparse-point install target: $target"
    }
    if ($PSCmdlet.ShouldProcess($target, 'Remove synology-drive-sync executable')) {
        Remove-Item -LiteralPath $target -Force
        Write-Host "Removed $target"
        Write-Host 'Scheduled tasks, configuration, logs, credentials, and user PATH entries were not removed.'
    }
    return
}

Add-Type -AssemblyName System.IO.Compression.FileSystem

$osArchitecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
$architecture = switch ($osArchitecture) {
    'X64' { 'x86_64'; break }
    'Arm64' { 'aarch64'; break }
    default { throw "Unsupported Windows architecture: $($_)" }
}

$headers = @{ 'User-Agent' = 'synology-drive-sync-installer' }
if ([string]::IsNullOrEmpty($Version)) {
    $release = Invoke-RestMethod -Headers $headers -Uri "https://api.github.com/repos/$Repository/releases/latest"
    $Version = [string] $release.tag_name
    if ($Version -notmatch '^[0-9]{2}\.[1-9][0-9]*$') {
        throw "Latest release tag is not calendar form YY.N: $Version"
    }
}

$asset = "synology-drive-sync-$Version-windows-$architecture.zip"
$releaseUrl = "https://github.com/$Repository/releases/download/$Version"
$tempDir = Join-Path ([IO.Path]::GetTempPath()) ("sdsync-install-" + [Guid]::NewGuid().ToString('N'))
[void] (New-Item -ItemType Directory -Path $tempDir)

try {
    $archivePath = Join-Path $tempDir $asset
    $checksumsPath = Join-Path $tempDir 'SHA256SUMS'
    Invoke-WebRequest -Headers $headers -Uri "$releaseUrl/$asset" -OutFile $archivePath
    Invoke-WebRequest -Headers $headers -Uri "$releaseUrl/SHA256SUMS" -OutFile $checksumsPath

    $escapedAsset = [Regex]::Escape($asset)
    $matches = @(Get-Content -LiteralPath $checksumsPath | Where-Object {
        $_ -match "^([0-9A-Fa-f]{64})\s+\*?$escapedAsset$"
    })
    if ($matches.Count -ne 1) {
        throw "SHA256SUMS must contain exactly one checksum for $asset."
    }
    [void] ($matches[0] -match '^([0-9A-Fa-f]{64})')
    $expected = $Matches[1].ToLowerInvariant()
    $actual = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "SHA-256 verification failed for $asset."
    }

    $archiveRoot = "synology-drive-sync-$Version-windows-$architecture"
    $allowedDirectories = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($name in @($archiveRoot, "$archiveRoot/completions", "$archiveRoot/man")) {
        [void] $allowedDirectories.Add($name)
    }
    $requiredFiles = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($name in @(
        "$archiveRoot/synology-drive-sync.exe",
        "$archiveRoot/LICENSE",
        "$archiveRoot/THIRD_PARTY_LICENSES.html",
        "$archiveRoot/README.md",
        "$archiveRoot/SECURITY.md",
        "$archiveRoot/completions/synology-drive-sync.bash",
        "$archiveRoot/completions/_synology-drive-sync",
        "$archiveRoot/completions/synology-drive-sync.fish",
        "$archiveRoot/completions/synology-drive-sync.ps1",
        "$archiveRoot/completions/synology-drive-sync.elv",
        "$archiveRoot/man/synology-drive-sync-completions.1",
        "$archiveRoot/man/synology-drive-sync-config-path.1",
        "$archiveRoot/man/synology-drive-sync-config-show.1",
        "$archiveRoot/man/synology-drive-sync-config-validate.1",
        "$archiveRoot/man/synology-drive-sync-config.1",
        "$archiveRoot/man/synology-drive-sync-credentials-remove.1",
        "$archiveRoot/man/synology-drive-sync-credentials-set-password.1",
        "$archiveRoot/man/synology-drive-sync-credentials-set-totp.1",
        "$archiveRoot/man/synology-drive-sync-credentials-status.1",
        "$archiveRoot/man/synology-drive-sync-credentials.1",
        "$archiveRoot/man/synology-drive-sync-doctor.1",
        "$archiveRoot/man/synology-drive-sync-doctor-source.1",
        "$archiveRoot/man/synology-drive-sync-doctor-target.1",
        "$archiveRoot/man/synology-drive-sync-manpage.1",
        "$archiveRoot/man/synology-drive-sync-plan.1",
        "$archiveRoot/man/synology-drive-sync-sync.1",
        "$archiveRoot/man/synology-drive-sync.1"
    )) {
        [void] $requiredFiles.Add($name)
    }

    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $seenFiles = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $binaryEntry = $null
    $zip = [IO.Compression.ZipFile]::OpenRead($archivePath)
    try {
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName
            if ([string]::IsNullOrEmpty($name) -or
                $name.StartsWith('/', [StringComparison]::Ordinal) -or
                $name.StartsWith('\', [StringComparison]::Ordinal) -or
                $name -match '^[A-Za-z]:' -or
                $name.Contains('\') -or
                $name -match '(^|/)\.\.(/|$)') {
                throw "Verified archive contains an unsafe member path: $name"
            }

            $isDirectory = $name.EndsWith('/', [StringComparison]::Ordinal)
            $canonical = $name.TrimEnd([char] '/')
            if (-not $seen.Add($canonical)) {
                throw "Verified archive contains a duplicate member: $canonical"
            }
            if (-not $allowedDirectories.Contains($canonical) -and
                -not $requiredFiles.Contains($canonical)) {
                throw "Verified archive contains an unexpected member: $name"
            }
            if ($isDirectory -ne $allowedDirectories.Contains($canonical)) {
                throw "Verified archive member has an unexpected file type: $name"
            }

            $unixType = (($entry.ExternalAttributes -shr 16) -band 0xF000)
            $dosAttributes = ($entry.ExternalAttributes -band 0xFFFF)
            if (($dosAttributes -band 0x0400) -ne 0 -or $unixType -eq 0xA000) {
                throw "Verified archive contains a symlink or reparse point: $name"
            }
            if ($isDirectory) {
                if ($entry.Length -ne 0 -or ($unixType -ne 0 -and $unixType -ne 0x4000)) {
                    throw "Verified archive contains an invalid directory entry: $name"
                }
                continue
            }
            if ($unixType -ne 0 -and $unixType -ne 0x8000) {
                throw "Verified archive contains an unsupported member type: $name"
            }
            [void] $seenFiles.Add($canonical)
            if ($canonical -eq "$archiveRoot/synology-drive-sync.exe") {
                $binaryEntry = $entry
            }
        }

        foreach ($required in $requiredFiles) {
            if (-not $seenFiles.Contains($required)) {
                throw "Verified archive is missing required member: $required"
            }
        }
        if ($null -eq $binaryEntry) {
            throw 'Verified archive did not contain the expected executable.'
        }

        $extractDir = Join-Path $tempDir 'extract'
        $candidateDir = Join-Path $extractDir $archiveRoot
        [void] (New-Item -ItemType Directory -Path $candidateDir)
        $candidate = Join-Path $candidateDir 'synology-drive-sync.exe'
        $input = $binaryEntry.Open()
        try {
            $output = [IO.File]::Create($candidate)
            try {
                $input.CopyTo($output)
            }
            finally {
                $output.Dispose()
            }
        }
        finally {
            $input.Dispose()
        }
    }
    finally {
        $zip.Dispose()
    }

    $candidate = Join-Path $extractDir "$archiveRoot\synology-drive-sync.exe"
    $candidateItem = Get-Item -LiteralPath $candidate -Force
    if ($candidateItem.PSIsContainer -or
        (($candidateItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw 'Verified archive did not contain the expected executable.'
    }
    $reportedVersionLines = @(& $candidate --version)
    $versionExitCode = $LASTEXITCODE
    $expectedVersion = "synology-drive-sync $Version"
    if ($versionExitCode -ne 0 -or
        $reportedVersionLines.Count -ne 1 -or
        ([string] $reportedVersionLines[0]) -cne $expectedVersion) {
        $reportedVersion = $reportedVersionLines -join "`n"
        throw "Archive binary version did not exactly match ${expectedVersion}: $reportedVersion"
    }

    if (Test-Path -LiteralPath $InstallDir) {
        $installItem = Get-Item -LiteralPath $InstallDir -Force
        if (-not $installItem.PSIsContainer -or
            (($installItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
            throw "Install directory is not a non-reparse directory: $InstallDir"
        }
    }
    elseif ($PSCmdlet.ShouldProcess($InstallDir, 'Create install directory')) {
        [void] (New-Item -ItemType Directory -Path $InstallDir)
    }
    else {
        return
    }
    $resolvedInstallDir = (Resolve-Path -LiteralPath $InstallDir).Path
    $target = Join-Path $resolvedInstallDir 'synology-drive-sync.exe'
    if (Test-Path -LiteralPath $target) {
        $targetItem = Get-Item -LiteralPath $target -Force
        if ($targetItem.PSIsContainer -or
            (($targetItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
            throw "Install target is a directory or reparse point: $target"
        }
    }
    if ($PSCmdlet.ShouldProcess($target, "Install or upgrade synology-drive-sync $Version")) {
        $staged = Join-Path $resolvedInstallDir ('.synology-drive-sync.install.' + [Guid]::NewGuid().ToString('N'))
        try {
            Copy-Item -LiteralPath $candidate -Destination $staged
            if ([IO.File]::Exists($target)) {
                [IO.File]::Replace($staged, $target, $null)
            }
            else {
                [IO.File]::Move($staged, $target)
            }
        }
        finally {
            if (Test-Path -LiteralPath $staged -PathType Leaf) {
                Remove-Item -LiteralPath $staged -Force
            }
        }

        if ($AddToUserPath -and $PSCmdlet.ShouldProcess('current-user PATH', "Add $resolvedInstallDir")) {
            $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
            $parts = @($userPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
            if ($parts -notcontains $resolvedInstallDir) {
                $updated = (@($parts) + $resolvedInstallDir) -join ';'
                [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
                Write-Host 'Updated the current user PATH; open a new terminal to use it.'
            }
        }

        Write-Host "Installed synology-drive-sync $Version to $target"
    }
}
finally {
    $expectedPrefix = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    $resolvedTemp = [IO.Path]::GetFullPath($tempDir)
    if (-not $resolvedTemp.StartsWith($expectedPrefix, [StringComparison]::OrdinalIgnoreCase) -or
        -not ([IO.Path]::GetFileName($resolvedTemp)).StartsWith('sdsync-install-', [StringComparison]::Ordinal)) {
        throw "Refusing to clean unexpected temporary path: $resolvedTemp"
    }
    if (Test-Path -LiteralPath $resolvedTemp -PathType Container) {
        Remove-Item -LiteralPath $resolvedTemp -Recurse -Force
    }
}
