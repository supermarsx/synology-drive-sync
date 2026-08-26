[CmdletBinding()]
param(
    [ValidateSet('Check', 'Report', 'Html')]
    [string]$Mode = 'Check'
)

$ErrorActionPreference = 'Stop'

function Assert-NativeSuccess {
    param([Parameter(Mandatory)][string]$Step)

    if ($LASTEXITCODE -ne 0) {
        throw "$Step failed with exit code $LASTEXITCODE."
    }
}

function Resolve-CoveragePython {
    foreach ($name in @('python3', 'python')) {
        $command = Get-Command $name -CommandType Application -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($null -eq $command) {
            continue
        }
        & $command.Source -c `
            'import sys; raise SystemExit(0 if sys.version_info >= (3, 8) else 1)' 2>$null
        if ($LASTEXITCODE -eq 0) {
            return $command.Source
        }
    }
    throw 'Python 3.8 or newer is required to validate the split coverage policy.'
}

function Invoke-CoveragePolicy {
    param(
        [Parameter(Mandatory)][string]$Python,
        [Parameter(Mandatory)][string]$SummaryPath,
        [Parameter(Mandatory)][string]$RepositoryRoot,
        [Parameter(Mandatory)][int]$GeneralMinimum,
        [Parameter(Mandatory)][int]$DsmMinimum,
        [Parameter(Mandatory)][bool]$EnforceThresholds
    )

    $policyScript = Join-Path (Join-Path $RepositoryRoot 'scripts') 'coverage_policy.py'
    $arguments = @(
        $policyScript,
        '--summary', $SummaryPath,
        '--repository', $RepositoryRoot,
        '--minimum-general', $GeneralMinimum,
        '--minimum-dsm', $DsmMinimum
    )
    if ($EnforceThresholds) {
        $arguments += '--enforce'
    }
    & $Python @arguments
    Assert-NativeSuccess 'Validating the split line-coverage policy'
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$configPath = Join-Path $repoRoot '.config\coverage.env'
$settings = @{}

foreach ($line in Get-Content -LiteralPath $configPath) {
    if ([string]::IsNullOrWhiteSpace($line) -or $line.StartsWith('#')) {
        continue
    }
    if ($line -cnotmatch '^([A-Z_]+)=(.+)$') {
        throw "Invalid coverage setting: $line"
    }
    if ($settings.ContainsKey($Matches[1])) {
        throw "Duplicate coverage setting: $($Matches[1])"
    }
    $settings[$Matches[1]] = $Matches[2]
}

$requiredSettings = @(
    'RUST_TOOLCHAIN',
    'CARGO_LLVM_COV_VERSION',
    'COVERAGE_MIN_LINES',
    'COVERAGE_DSM_MIN_LINES'
)
foreach ($name in $requiredSettings) {
    if (-not $settings.ContainsKey($name)) {
        throw "Missing coverage setting: $name"
    }
}

$rustToolchain = $settings['RUST_TOOLCHAIN']
$toolVersion = $settings['CARGO_LLVM_COV_VERSION']
$minimumLines = $settings['COVERAGE_MIN_LINES']
$dsmMinimumLines = $settings['COVERAGE_DSM_MIN_LINES']
if ($rustToolchain -notmatch '^\d+\.\d+\.\d+$' -or
    $toolVersion -notmatch '^\d+\.\d+\.\d+$' -or
    $minimumLines -notmatch '^([1-9]\d?|100)$' -or
    $dsmMinimumLines -notmatch '^([1-9]\d?|100)$') {
    throw "Invalid coverage configuration in $configPath"
}
$coveragePython = Resolve-CoveragePython

Push-Location $repoRoot
try {
    $actualVersionLines = & cargo "+$rustToolchain" llvm-cov --version 2>$null
    $versionExitCode = $LASTEXITCODE
    $actualVersion = ($actualVersionLines | Out-String).Trim()
    $expectedVersion = "cargo-llvm-cov $toolVersion"
    if ($versionExitCode -ne 0 -or $actualVersion -ne $expectedVersion) {
        throw @"
Expected $expectedVersion for Rust $rustToolchain, found: $(if ($actualVersion) { $actualVersion } else { 'not installed' })
Install it with:
  cargo +$rustToolchain install cargo-llvm-cov --version $toolVersion --locked
"@
    }

    $installedComponents = & rustup component list --toolchain $rustToolchain --installed
    Assert-NativeSuccess 'Listing installed Rust components'
    if (-not ($installedComponents -match '^llvm-tools')) {
        throw @"
The llvm-tools-preview component is required for Rust $rustToolchain.
Install it with:
  rustup component add --toolchain $rustToolchain llvm-tools-preview
"@
    }

    $outputDir = Join-Path $repoRoot 'target\llvm-cov'
    $summaryPath = Join-Path $outputDir 'coverage-summary.json'

    & cargo "+$rustToolchain" llvm-cov clean --workspace
    Assert-NativeSuccess 'Cleaning prior coverage instrumentation'

    & cargo "+$rustToolchain" llvm-cov --locked --workspace --all-targets --no-report
    Assert-NativeSuccess 'Running instrumented tests'

    New-Item -ItemType Directory -Path $outputDir -Force | Out-Null
    & cargo "+$rustToolchain" llvm-cov report `
        --json `
        --summary-only `
        --output-path $summaryPath
    Assert-NativeSuccess 'Writing the JSON coverage summary'

    switch ($Mode) {
        'Check' {
            & cargo "+$rustToolchain" llvm-cov report
            Assert-NativeSuccess 'Printing the unfiltered coverage report'
            Invoke-CoveragePolicy `
                -Python $coveragePython `
                -SummaryPath $summaryPath `
                -RepositoryRoot $repoRoot `
                -GeneralMinimum ([int]$minimumLines) `
                -DsmMinimum ([int]$dsmMinimumLines) `
                -EnforceThresholds $true
        }
        'Report' {
            & cargo "+$rustToolchain" llvm-cov report
            Assert-NativeSuccess 'Printing the coverage report'
            Invoke-CoveragePolicy `
                -Python $coveragePython `
                -SummaryPath $summaryPath `
                -RepositoryRoot $repoRoot `
                -GeneralMinimum ([int]$minimumLines) `
                -DsmMinimum ([int]$dsmMinimumLines) `
                -EnforceThresholds $false
        }
        'Html' {
            $htmlDir = Join-Path $outputDir 'html'
            & cargo "+$rustToolchain" llvm-cov report --html --output-dir $htmlDir
            Assert-NativeSuccess 'Writing the HTML coverage report'
            Invoke-CoveragePolicy `
                -Python $coveragePython `
                -SummaryPath $summaryPath `
                -RepositoryRoot $repoRoot `
                -GeneralMinimum ([int]$minimumLines) `
                -DsmMinimum ([int]$dsmMinimumLines) `
                -EnforceThresholds $false
            Write-Output "HTML coverage report: $(Join-Path $htmlDir 'index.html')"
        }
    }

    Write-Output "Coverage summary: $summaryPath"
}
finally {
    Pop-Location
}
