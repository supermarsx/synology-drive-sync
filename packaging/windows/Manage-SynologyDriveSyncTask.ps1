[CmdletBinding(SupportsShouldProcess)]
param(
    [Parameter(Mandatory, Position = 0)]
    [ValidateSet('status', 'start', 'stop', 'restart', 'enable', 'disable', 'logs', 'diagnostics', 'uninstall')]
    [string] $Action,

    [ValidateNotNullOrEmpty()]
    [string] $TaskName = 'Synology Drive Sync',

    [ValidateNotNullOrEmpty()]
    [string] $LogFile = (Join-Path $env:LOCALAPPDATA 'synology-drive-sync\logs\sync.log'),

    [ValidateRange(1, 10000)]
    [int] $Tail = 100,

    [ValidateRange(1, 600)]
    [int] $StopTimeoutSeconds = 120
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Get-ManagedTask {
    Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
}

function Wait-ManagedTaskStopped {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($timer.Elapsed.TotalSeconds -lt $StopTimeoutSeconds) {
        $task = Get-ManagedTask
        if ($null -eq $task -or $task.State -ne 'Running') {
            return
        }
        Start-Sleep -Seconds 1
    }
    throw "Task '$TaskName' did not stop within $StopTimeoutSeconds seconds; it was not force-terminated."
}

function Show-Status {
    $task = Get-ManagedTask
    if ($null -eq $task) {
        [pscustomobject]@{
            TaskName = $TaskName
            Installed = $false
            State = 'Absent'
            Enabled = $false
            LastRunTime = $null
            LastTaskResult = $null
            NextRunTime = $null
        }
        return
    }
    $info = Get-ScheduledTaskInfo -TaskName $TaskName
    [pscustomobject]@{
        TaskName = $TaskName
        Installed = $true
        State = [string] $task.State
        Enabled = $task.State -ne 'Disabled'
        LastRunTime = $info.LastRunTime
        LastTaskResult = $info.LastTaskResult
        NextRunTime = $info.NextRunTime
        MissedRuns = $info.NumberOfMissedRuns
    }
}

function Show-Logs {
    $resolvedLog = [IO.Path]::GetFullPath($LogFile)
    if (-not (Test-Path -LiteralPath $resolvedLog)) {
        Write-Host "Log is absent: $resolvedLog"
        return
    }
    $item = Get-Item -LiteralPath $resolvedLog -Force
    if ($item.PSIsContainer -or
        (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
        throw "Log is a directory or reparse point: $resolvedLog"
    }
    Get-Content -LiteralPath $resolvedLog -Tail $Tail
}

$task = Get-ManagedTask
if ($null -eq $task) {
    if ($Action -eq 'status' -or $Action -eq 'diagnostics') {
        Show-Status
        if ($Action -eq 'diagnostics') { Show-Logs }
        return
    }
    if ($Action -eq 'uninstall') {
        Write-Host "Task '$TaskName' is already absent."
        return
    }
    throw "Task '$TaskName' is not installed."
}

switch ($Action) {
    'status' { Show-Status }
    'logs' { Show-Logs }
    'diagnostics' {
        Show-Status
        Show-Logs
    }
    'start' {
        if ($task.State -eq 'Running') {
            Write-Host "Task '$TaskName' is already running."
        }
        elseif ($PSCmdlet.ShouldProcess($TaskName, 'Start scheduled task')) {
            Start-ScheduledTask -TaskName $TaskName
        }
    }
    'stop' {
        if ($task.State -ne 'Running') {
            Write-Host "Task '$TaskName' is already stopped."
        }
        elseif ($PSCmdlet.ShouldProcess($TaskName, 'Request scheduled task stop')) {
            Stop-ScheduledTask -TaskName $TaskName
            Wait-ManagedTaskStopped
        }
    }
    'restart' {
        if ($PSCmdlet.ShouldProcess($TaskName, 'Stop and restart scheduled task')) {
            if ($task.State -eq 'Running') {
                Stop-ScheduledTask -TaskName $TaskName
                Wait-ManagedTaskStopped
            }
            Start-ScheduledTask -TaskName $TaskName
        }
    }
    'enable' {
        if ($task.State -eq 'Disabled') {
            if ($PSCmdlet.ShouldProcess($TaskName, 'Enable scheduled task')) {
                Enable-ScheduledTask -TaskName $TaskName | Out-Null
            }
        }
        else {
            Write-Host "Task '$TaskName' is already enabled."
        }
    }
    'disable' {
        if ($task.State -eq 'Disabled') {
            Write-Host "Task '$TaskName' is already disabled."
        }
        elseif ($PSCmdlet.ShouldProcess($TaskName, 'Disable future scheduled starts')) {
            Disable-ScheduledTask -TaskName $TaskName | Out-Null
        }
    }
    'uninstall' {
        if ($PSCmdlet.ShouldProcess($TaskName, 'Stop and unregister scheduled task')) {
            if ($task.State -eq 'Running') {
                Stop-ScheduledTask -TaskName $TaskName
                Wait-ManagedTaskStopped
            }
            Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
            Write-Host 'Task removed. Logs and Credential Manager entries were retained.'
        }
    }
}
