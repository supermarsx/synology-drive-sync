[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$scriptFiles = @(git ls-files --cached --others --exclude-standard -- '*.ps1')
if ($LASTEXITCODE -ne 0) {
    throw 'git ls-files failed while locating PowerShell scripts.'
}
if ($scriptFiles.Count -eq 0) {
    throw 'No PowerShell scripts found to validate.'
}

$failures = [Collections.Generic.List[string]]::new()
foreach ($scriptFile in $scriptFiles) {
    $tokens = $null
    $parseErrors = $null
    [void] [Management.Automation.Language.Parser]::ParseFile(
        (Resolve-Path -LiteralPath $scriptFile).Path,
        [ref] $tokens,
        [ref] $parseErrors
    )
    foreach ($parseError in $parseErrors) {
        $failures.Add("${scriptFile}: $($parseError.Message)")
    }
}

if ($failures.Count -gt 0) {
    throw ($failures -join [Environment]::NewLine)
}

Write-Host "Parsed $($scriptFiles.Count) PowerShell scripts successfully."
