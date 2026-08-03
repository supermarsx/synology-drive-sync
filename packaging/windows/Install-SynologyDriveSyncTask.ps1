[CmdletBinding(SupportsShouldProcess)]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $Source,

    [Parameter(Mandatory)]
    [ValidatePattern('^/[^\r\n]+$')]
    [string] $Remote,

    [Parameter(Mandatory)]
    [ValidatePattern('^https://')]
    [string] $Url,

    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $Username,

    [string] $Executable = (Join-Path $env:LOCALAPPDATA 'Programs\synology-drive-sync\synology-drive-sync.exe'),
    [string] $TaskName = 'Synology Drive Sync',
    [datetime] $At = [datetime]::Today.AddHours(3),

    [ValidateRange(1, 16)]
    [int] $Jobs = 2,

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
$resolvedSource = (Resolve-Path -LiteralPath $Source).Path
if (-not (Test-Path -LiteralPath $resolvedSource -PathType Container)) {
    throw "Source is not a directory: $resolvedSource"
}

$arguments = [System.Collections.Generic.List[string]]::new()
$arguments.Add('sync')
$arguments.Add($resolvedSource)
$arguments.Add($Remote)
$arguments.Add('--url')
$arguments.Add($Url)
$arguments.Add('--username')
$arguments.Add($Username)
$arguments.Add('--jobs')
$arguments.Add($Jobs.ToString([Globalization.CultureInfo]::InvariantCulture))
if ($Delete) {
    $arguments.Add('--delete')
    $arguments.Add('--max-delete')
    $arguments.Add($MaxDelete.ToString([Globalization.CultureInfo]::InvariantCulture))
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
    -ExecutionTimeLimit (New-TimeSpan -Hours 3)
$identity = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
$principal = New-ScheduledTaskPrincipal -UserId $identity -LogonType Interactive -RunLevel Limited
$task = New-ScheduledTask `
    -Action $action `
    -Trigger $trigger `
    -Settings $settings `
    -Principal $principal `
    -Description 'One-way local folder sync to Synology Drive through File Station WebAPI.'

if ($PSCmdlet.ShouldProcess($TaskName, 'Register current-user scheduled task')) {
    Register-ScheduledTask -TaskName $TaskName -InputObject $task -Force:$Force | Out-Null
    Write-Host "Registered '$TaskName' for $($At.ToString('HH:mm')) as $identity."
    Write-Host "The task uses the current user's Windows Credential Manager entries; no secret is stored in its arguments."
}
