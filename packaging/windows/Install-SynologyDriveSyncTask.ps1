[CmdletBinding(SupportsShouldProcess)]
param(
    [ValidateNotNullOrEmpty()]
    [string] $Source,

    [ValidatePattern('^/[^\r\n]+$')]
    [string] $Remote,

    [ValidatePattern('^https://')]
    [string] $Url,

    [ValidateNotNullOrEmpty()]
    [string] $Username,

    [string] $Config,
    [string] $Profile,
    [string[]] $Profiles,
    [switch] $AllProfiles,

    [ValidateRange(0, 2147483647)]
    [Nullable[int]] $MaxTotalDelete,

    [string] $Executable = (Join-Path $env:LOCALAPPDATA 'Programs\synology-drive-sync\synology-drive-sync.exe'),
    [string] $TaskName = 'Synology Drive Sync',
    [datetime] $At = [datetime]::Today.AddHours(3),

    [ValidateRange(1, 16)]
    [int] $Jobs = 2,

    [ValidateNotNullOrEmpty()]
    [string] $LogFile = (Join-Path $env:LOCALAPPDATA 'synology-drive-sync\logs\sync.log'),

    [ValidateRange(1, 168)]
    [int] $ExecutionTimeLimitHours = 24,

    [switch] $Delete,

    [ValidateRange(0, 2147483647)]
    [int] $MaxDelete = 100,

    [switch] $Force
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function ConvertTo-NativeArgument {
    param([AllowEmptyString()][string] $Value)

    if ($Value.Length -gt 0 -and $Value -notmatch '[\s"]') {
        return $Value
    }

    $builder = [System.Text.StringBuilder]::new()
    [void] $builder.Append('"')
    $backslashes = 0
    foreach ($character in $Value.ToCharArray()) {
        if ($character -eq '\') {
            $backslashes++
            continue
        }
        if ($character -eq '"') {
            [void] $builder.Append(('\' * (($backslashes * 2) + 1)))
            [void] $builder.Append('"')
            $backslashes = 0
            continue
        }
        if ($backslashes -gt 0) {
            [void] $builder.Append(('\' * $backslashes))
            $backslashes = 0
        }
        [void] $builder.Append($character)
    }
    if ($backslashes -gt 0) {
        [void] $builder.Append(('\' * ($backslashes * 2)))
    }
    [void] $builder.Append('"')
    return $builder.ToString()
}

$resolvedExecutable = (Resolve-Path -LiteralPath $Executable).Path
$executableItem = Get-Item -LiteralPath $resolvedExecutable -Force
if ($executableItem.PSIsContainer -or
    (($executableItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
    throw "Executable is not a non-reparse regular file: $resolvedExecutable"
}

$selectedProfiles = @($Profiles)
$usesBatch = $AllProfiles -or $selectedProfiles.Count -gt 0
if ($AllProfiles -and $selectedProfiles.Count -gt 0) {
    throw '-Profiles and -AllProfiles cannot be combined.'
}
if ($usesBatch -and -not [string]::IsNullOrWhiteSpace($Profile)) {
    throw '-Profile cannot be combined with -Profiles or -AllProfiles.'
}
if (($usesBatch -or -not [string]::IsNullOrWhiteSpace($Profile)) -and
    [string]::IsNullOrWhiteSpace($Config)) {
    throw 'Profile selection requires -Config.'
}
if ($null -ne $MaxTotalDelete -and -not $usesBatch) {
    throw '-MaxTotalDelete requires -Profiles or -AllProfiles.'
}

$usesConfig = -not [string]::IsNullOrWhiteSpace($Config)
if ($usesConfig) {
    foreach ($directValue in @($Source, $Remote, $Url, $Username)) {
        if (-not [string]::IsNullOrWhiteSpace($directValue)) {
            throw '-Config profile jobs cannot be combined with -Source, -Remote, -Url, or -Username.'
        }
    }
    $resolvedConfig = (Resolve-Path -LiteralPath $Config).Path
    $configItem = Get-Item -LiteralPath $resolvedConfig -Force
    if ($configItem.PSIsContainer -or
        (($configItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "Config is not a non-reparse regular file: $resolvedConfig"
    }
    & $resolvedExecutable --config $resolvedConfig --quiet config validate
    if ($LASTEXITCODE -ne 0) {
        throw "Configuration validation failed with exit code $LASTEXITCODE."
    }
}
else {
    foreach ($required in @{
        Source = $Source
        Remote = $Remote
        Url = $Url
        Username = $Username
    }.GetEnumerator()) {
        if ([string]::IsNullOrWhiteSpace([string] $required.Value)) {
            throw "-$($required.Key) is required without -Config."
        }
    }
    $resolvedSource = (Resolve-Path -LiteralPath $Source).Path
    $sourceItem = Get-Item -LiteralPath $resolvedSource -Force
    if (-not $sourceItem.PSIsContainer -or
        (($sourceItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "Source is not a non-reparse directory: $resolvedSource"
    }
}

$logLeaf = Split-Path -Leaf $LogFile
$logParent = Split-Path -Parent $LogFile
if ([string]::IsNullOrWhiteSpace($logLeaf) -or [string]::IsNullOrWhiteSpace($logParent)) {
    throw "LogFile must include a directory and file name: $LogFile"
}
$resolvedLogFile = [IO.Path]::GetFullPath($LogFile)
$resolvedLogParent = Split-Path -Parent $resolvedLogFile
if (Test-Path -LiteralPath $resolvedLogFile) {
    $logItem = Get-Item -LiteralPath $resolvedLogFile -Force
    if ($logItem.PSIsContainer -or
        (($logItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "Log target is a directory or reparse point: $resolvedLogFile"
    }
}

$arguments = [System.Collections.Generic.List[string]]::new()
if ($usesConfig) {
    $arguments.Add('--config')
    $arguments.Add($resolvedConfig)
    if (-not [string]::IsNullOrWhiteSpace($Profile)) {
        $arguments.Add('--profile')
        $arguments.Add($Profile)
    }
    $arguments.Add('sync')
    if ($AllProfiles) {
        $arguments.Add('--all-profiles')
    }
    elseif ($selectedProfiles.Count -gt 0) {
        if ($selectedProfiles | Where-Object { [string]::IsNullOrWhiteSpace($_) -or $_.Contains(',') }) {
            throw '-Profiles entries must be non-empty and cannot contain commas.'
        }
        $arguments.Add('--profiles')
        $arguments.Add(($selectedProfiles -join ','))
    }
}
else {
    $arguments.Add('sync')
    $arguments.Add($resolvedSource)
    $arguments.Add($Remote)
    $arguments.Add('--url')
    $arguments.Add($Url)
    $arguments.Add('--username')
    $arguments.Add($Username)
}
$arguments.Add('--jobs')
$arguments.Add($Jobs.ToString([Globalization.CultureInfo]::InvariantCulture))
$arguments.Add('--quiet')
$arguments.Add('--log-format')
$arguments.Add('json')
$arguments.Add('--log-file')
$arguments.Add($resolvedLogFile)
$arguments.Add('--progress')
$arguments.Add('never')
if ($Delete) {
    $arguments.Add('--delete')
    $arguments.Add('--max-delete')
    $arguments.Add($MaxDelete.ToString([Globalization.CultureInfo]::InvariantCulture))
}
else {
    # A scheduled job must not inherit delete=true from a profile accidentally.
    $arguments.Add('--no-delete')
}
if ($null -ne $MaxTotalDelete) {
    $arguments.Add('--max-total-delete')
    $arguments.Add($MaxTotalDelete.ToString([Globalization.CultureInfo]::InvariantCulture))
}

$argumentLine = ($arguments | ForEach-Object { ConvertTo-NativeArgument $_ }) -join ' '
$existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if ($null -ne $existing -and -not $Force) {
    throw "Task '$TaskName' already exists. Pass -Force to replace it intentionally."
}

$action = New-ScheduledTaskAction -Execute $resolvedExecutable -Argument $argumentLine -WorkingDirectory (Split-Path -Parent $resolvedExecutable)
$trigger = New-ScheduledTaskTrigger -Daily -At $At
$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -StartWhenAvailable `
    -MultipleInstances IgnoreNew `
    -ExecutionTimeLimit (New-TimeSpan -Hours $ExecutionTimeLimitHours)
$identity = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
$principal = New-ScheduledTaskPrincipal -UserId $identity -LogonType Interactive -RunLevel Limited
$task = New-ScheduledTask `
    -Action $action `
    -Trigger $trigger `
    -Settings $settings `
    -Principal $principal `
    -Description 'One-way local folder sync to Synology Drive through File Station WebAPI.'

if ($PSCmdlet.ShouldProcess($TaskName, 'Register current-user scheduled task')) {
    [void] (New-Item -ItemType Directory -Path $resolvedLogParent -Force)
    Register-ScheduledTask -TaskName $TaskName -InputObject $task -Force:$Force | Out-Null
    Write-Host "Registered '$TaskName' for $($At.ToString('HH:mm')) as $identity."
    Write-Host "The task uses the current user's Windows Credential Manager entries; no secret is stored in its arguments."
    Write-Host "Structured logs rotate at $resolvedLogFile; monitor LastTaskResult for nonzero failures."
}
