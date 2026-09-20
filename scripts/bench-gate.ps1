#requires -Version 7
<#
.SYNOPSIS
    Performance regression gate over the runtime's turn_overhead probe.

.DESCRIPTION
    Runs `cargo test --release ... turn_overhead` and either saves the numbers
    as a machine-local baseline or compares the current numbers against one.
    Any metric more than $Threshold slower fails with exit code 1, so a CI job
    (or a pre-commit hook) can block silent performance regressions.

    Baselines are machine-specific: never commit one to the repo.

.USAGE
    Save a baseline before starting work:
        ./scripts/bench-gate.ps1 -Save
    Gate the current tree against it:
        ./scripts/bench-gate.ps1
    Custom baseline path and stricter threshold:
        ./scripts/bench-gate.ps1 -BaselinePath a.json -Threshold 0.20
#>
param(
    [string]$BaselinePath = "$env:TEMP/heartflow-perf-baseline.json",
    [double]$Threshold = 0.30,
    [switch]$Save
)

$ErrorActionPreference = 'Stop'
Push-Location "$PSScriptRoot/.."
try {
    Write-Host 'running turn_overhead probe (release)...'
    $output = cargo test --release -p heartflow-runtime --lib -- --ignored --nocapture turn_overhead 2>&1 |
        Out-String
    if ($LASTEXITCODE -ne 0) {
        Write-Host $output
        throw 'benchmark probe failed to run'
    }
}
finally {
    Pop-Location
}

# Parse the probe output into @{ rounds = @{ metric = microseconds } }.
# `timed` metrics print "<n> us"; compact_session prints a Debug Duration
# (s/ms/us), so every unit is normalized to microseconds.
$bench = [ordered]@{}
$group = $null
foreach ($line in ($output -split "`r?`n")) {
    if ($line -match 'rounds=(\d+)') {
        $group = $Matches[1]
        $bench[$group] = [ordered]@{}
    }
    elseif ($group -and $line -match '^\s+([a-z_]+)\s+([\d.]+)\s*(us|µs|ms|s)\s*$') {
        $value = [double]$Matches[2]
        $value *= switch ($Matches[3]) {
            's'  { 1000000 }
            'ms' { 1000 }
            default { 1 }   # us and µs are already the target unit
        }
        $bench[$group][$Matches[1]] = [math]::Round($value, 1)
    }
}
if ($bench.Count -eq 0) {
    throw 'no benchmark groups parsed; probe output changed?'
}

if ($Save) {
    $bench | ConvertTo-Json -Depth 3 | Set-Content $BaselinePath
    Write-Host "baseline saved: $BaselinePath"
    exit 0
}

if (-not (Test-Path $BaselinePath)) {
    Write-Host "no baseline at $BaselinePath - create one first with -Save" -ForegroundColor Yellow
    exit 2
}
$baseline = Get-Content $BaselinePath -Raw | ConvertFrom-Json

$failures = @()
foreach ($group in $baseline.PSObject.Properties.Name) {
    if (-not $bench.Contains($group)) {
        $failures += "group rounds=$group missing from current run"
        continue
    }
    foreach ($metric in $baseline.$group.PSObject.Properties.Name) {
        $old = [double]$baseline.$group.$metric
        $new = $bench[$group][$metric]
        if ($null -eq $new) {
            $failures += "rounds=$group/$metric missing from current run"
            continue
        }
        if ($new -gt $old * (1 + $Threshold)) {
            $pct = [math]::Round(100 * ($new - $old) / $old)
            $failures += "rounds=$group/${metric}: ${new}us vs baseline ${old}us (+$pct%)"
        }
    }
}

if ($failures.Count -gt 0) {
    Write-Host 'PERFORMANCE REGRESSION:' -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }
    exit 1
}
Write-Host "PASS: no metric degraded beyond $([math]::Round($Threshold * 100))% ($BaselinePath)" -ForegroundColor Green
