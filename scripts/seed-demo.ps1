<#
.SYNOPSIS
    Rebuild the demo model from its CSV + SQL sources and verify its content fingerprint.

.DESCRIPTION
    Demo/model data is defined only by CSV + SQL; `.bin` files are generated outputs and are never
    migrated or upgraded. This script is the reusable entry point for that rule:

      1. (optionally) builds target\debug\olap_engine.exe (offline);
      2. runs the model seed (`examples\superstore-v2.sql`) against a NEW database file;
      3. reads the generated file's header/format version;
      4. computes its content fingerprint with scripts\verify-model.ps1 and compares it with the
         pinned fingerprint in the model manifest.

    Exit 0 = seeded and fingerprint matches. Exit 1 = seed/verify failure or fingerprint mismatch.
    The output file is disposable; point -DatabaseFile at a scratch path (default: database.demo.bin).
    Never run this against shared\database.bin while the shared server is running.

.EXAMPLE
    .\scripts\seed-demo.ps1

.EXAMPLE
    .\scripts\seed-demo.ps1 -DatabaseFile C:\scratch\v2.bin -SkipBuild
#>
[CmdletBinding()]
param(
    [string]$DatabaseFile = 'database.demo.bin',
    [string]$ManifestPath = 'model-manifest.v2.json',
    [string]$EngineExe = '',
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$engineDir = Split-Path -Parent $PSScriptRoot
Push-Location $engineDir
try {
    if (-not $EngineExe) { $EngineExe = Join-Path $engineDir 'target\debug\olap_engine.exe' }

    if (-not $SkipBuild) {
        Write-Host 'Building olap_engine (offline)...'
        cargo build --offline --bin olap_engine
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
    }
    if (-not (Test-Path $EngineExe)) { throw "engine binary not found: $EngineExe (run without -SkipBuild?)" }
    if (-not (Test-Path $ManifestPath)) { throw "model manifest not found: $ManifestPath" }

    $seed = Join-Path $engineDir 'examples\superstore-v2.sql'
    if (-not (Test-Path $seed)) { throw "seed not found: $seed" }

    # The output is generated demo data: recreate it, never append/upgrade.
    if (Test-Path $DatabaseFile) { Remove-Item -LiteralPath $DatabaseFile -Force }
    $parent = Split-Path -Parent $DatabaseFile
    if ($parent -and -not (Test-Path $parent)) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }

    Write-Host "Seeding $DatabaseFile from examples\superstore-v2.sql ..."
    & $EngineExe --db $DatabaseFile $seed
    if ($LASTEXITCODE -ne 0) { throw "seed failed (exit $LASTEXITCODE)" }
    if (-not (Test-Path $DatabaseFile)) { throw "seed reported success but produced no file" }

    # Header: "OLAP" + little-endian u32 format version.
    $bytes = [System.IO.File]::ReadAllBytes($DatabaseFile)
    if ($bytes.Length -lt 8) { throw 'generated file is too short to be a catalogue' }
    $magic = -join ($bytes[0..3] | ForEach-Object { [char]$_ })
    if ($magic -ne 'OLAP') { throw "generated file has wrong magic '$magic'" }
    $format = [BitConverter]::ToUInt32($bytes, 4)

    # Fingerprint from the canonical verifier (same definition as the model pin).
    $verifier = Join-Path $engineDir 'scripts\verify-model.ps1'
    $line = & powershell -NoProfile -ExecutionPolicy Bypass -File $verifier -DatabaseFile $DatabaseFile -PrintFingerprint 2>&1 |
        Select-Object -Last 1
    if ($line -match 'fingerprint=([0-9A-Fa-f]{64})') {
        $found = $Matches[1].ToUpperInvariant()
    } elseif ($line -match '^\s*([0-9A-Fa-f]{64})\s*$') {
        $found = $Matches[1].ToUpperInvariant()
    } else {
        throw "could not read the content fingerprint; verifier said: $line"
    }

    $manifest = Get-Content $ManifestPath -Raw | ConvertFrom-Json
    $pinned = ([string]$manifest.file.contentFingerprint).ToUpperInvariant()
    $size = (Get-Item -LiteralPath $DatabaseFile).Length

    if ($found -ne $pinned) {
        Write-Host "MISMATCH: generated $found"
        Write-Host "          pinned   $pinned"
        throw 'regenerated model does not match the pinned content fingerprint'
    }

    Write-Host "OK: $DatabaseFile seeded from CSV+SQL (format $format, $size bytes)"
    Write-Host "    content fingerprint matches the model pin: $found"
    Write-Host '    Note: file bytes/SHA are not reproducible; the fingerprint is the model identity.'
    exit 0
}
catch {
    Write-Error $_.Exception.Message
    exit 1
}
finally {
    Pop-Location
}
