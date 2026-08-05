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

$repoRoot = Split-Path -Parent $PSScriptRoot
$configPath = Join-Path $repoRoot '.config\coverage.env'
$settings = @{}

foreach ($line in Get-Content -LiteralPath $configPath) {
    if ([string]::IsNullOrWhiteSpace($line) -or $line.StartsWith('#')) {
        continue
    }
    if ($line -notmatch '^([A-Z_]+)=(.+)$') {
        throw "Invalid coverage setting: $line"
    }
    $settings[$Matches[1]] = $Matches[2]
}

$requiredSettings = @('RUST_TOOLCHAIN', 'CARGO_LLVM_COV_VERSION', 'COVERAGE_MIN_LINES')
foreach ($name in $requiredSettings) {
    if (-not $settings.ContainsKey($name)) {
        throw "Missing coverage setting: $name"
    }
}

$rustToolchain = $settings['RUST_TOOLCHAIN']
$toolVersion = $settings['CARGO_LLVM_COV_VERSION']
$minimumLines = $settings['COVERAGE_MIN_LINES']
if ($rustToolchain -notmatch '^\d+\.\d+\.\d+$' -or
    $toolVersion -notmatch '^\d+\.\d+\.\d+$' -or
    $minimumLines -notmatch '^([1-9]\d?|100)$') {
    throw "Invalid coverage configuration in $configPath"
}

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
            & cargo "+$rustToolchain" llvm-cov report --fail-under-lines $minimumLines
            Assert-NativeSuccess "Enforcing the $minimumLines percent line-coverage gate"
        }
        'Report' {
            & cargo "+$rustToolchain" llvm-cov report
            Assert-NativeSuccess 'Printing the coverage report'
        }
        'Html' {
            $htmlDir = Join-Path $outputDir 'html'
            & cargo "+$rustToolchain" llvm-cov report --html --output-dir $htmlDir
            Assert-NativeSuccess 'Writing the HTML coverage report'
            Write-Output "HTML coverage report: $(Join-Path $htmlDir 'index.html')"
        }
    }

    Write-Output "Coverage summary: $summaryPath"
}
finally {
    Pop-Location
}
