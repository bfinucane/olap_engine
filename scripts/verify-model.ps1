<#
  verify-model.ps1 — assert that a database file, or a running engine, IS the pinned test model.

  WHY: "the test data and the database version must be clearly defined." A name is not a
  definition. This script compares what it finds against model-manifest.v1.json, which pins:
    * the EXACT frozen artefact  (file size + SHA-256),
    * the MODEL CONTENT          (content fingerprint; regeneration-stable),
    * the ENGINE identity        (package version, commit, protocol version, storage format),
    * the DATASETS               (names, columns, kinds, cardinalities, row counts),
    * behavioural checks         (Sales by Region, grand total, leaf-group count).

  Bytes of a REGENERATED seed are not reproducible (bincode/HashMap ordering), so:
    * a FILE check requires the exact pinned SHA-256 (that is what a frozen release is), and
      reports the content fingerprint as corroboration;
    * a LIVE check (a running server) requires the dataset/column/row-count/behavioural
      manifest only — it cannot see the file hash.

  USAGE
      .\scripts\verify-model.ps1 -DatabaseFile ..\shared\database.bin
      .\scripts\verify-model.ps1 -DatabaseFile ..\shared\database.bin -Regenerate
      .\scripts\verify-model.ps1 -BaseUrl http://127.0.0.1:8080/v1 [-Token <bearer>]

  Exit: 0 = matches the pinned model; 1 = mismatch (details printed); 2 = setup failure.
#>
param(
  [string] $DatabaseFile,
  [string] $BaseUrl,
  [string] $Token,
  [string] $EngineExe,
  [switch] $Regenerate
)

$ErrorActionPreference = 'Stop'
$EngineDir = Split-Path -Parent $PSScriptRoot
$ManifestPath = Join-Path $EngineDir 'model-manifest.v1.json'
if (-not (Test-Path $ManifestPath)) { Write-Host "SETUP: no manifest at $ManifestPath"; exit 2 }
$m = Get-Content $ManifestPath -Raw | ConvertFrom-Json
if (-not $EngineExe) { $EngineExe = Join-Path $EngineDir 'target\debug\olap_engine.exe' }

$failures = New-Object 'System.Collections.Generic.List[string]'
function Check([string]$label, [bool]$ok, [string]$detail) {
    if ($ok) { Write-Host "  PASS $label" }
    else {
        Write-Host "  FAIL $label : $detail"
        [void]$failures.Add("$label : $detail")
    }
}
function CloseEnough($a, $b) { return [Math]::Abs([double]$a - [double]$b) -le 0.005 }

Write-Host ""
Write-Host "  model: $($m.modelName) $($m.modelVersion)  (engine $($m.engine.packageVersion) @ $($m.engine.commit), contract $($m.contract))"
Write-Host ""

# ---------------------------------------------------------------------------
# FILE MODE — exact artefact + content fingerprint (+ optional seed regeneration)
# ---------------------------------------------------------------------------
if ($DatabaseFile) {
    if (-not (Test-Path $DatabaseFile)) { Write-Host "SETUP: no such database file: $DatabaseFile"; exit 2 }
    $size = (Get-Item $DatabaseFile).Length
    $sha = (Get-FileHash $DatabaseFile -Algorithm SHA256).Hash
    Check "file size == $($m.file.size) B" ($size -eq [int]$m.file.size) "found $size B"
    Check "file SHA-256 == pinned release" ($sha -eq $m.file.sha256) "found $sha"
    $fpLine = & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'content-fingerprint.ps1') -DatabaseFile $DatabaseFile -EngineExe $EngineExe 2>&1 | Select-Object -Last 1
    $fp = ($fpLine -split ' ')[0] -replace '^fingerprint=',''
    Check "content fingerprint == pinned model" ($fp -eq $m.file.contentFingerprint) "found $fp"
}

# ---------------------------------------------------------------------------
# REGENERATION MODE — the manifest must be reproducible from the seed (content)
# ---------------------------------------------------------------------------
if ($Regenerate) {
    $tmp = Join-Path $env:TEMP ("olap-regen-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    try {
        Push-Location $EngineDir
        try { & $EngineExe --db (Join-Path $tmp 'regen.bin') 'examples\Plateau\seed.sql' *> (Join-Path $tmp 'seed.log') }
        finally { Pop-Location }
        $exit = $LASTEXITCODE
        Check 'seed regeneration exits 0' ($exit -eq 0) "exit $exit (see $tmp\seed.log)"
        $fpLine = & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'content-fingerprint.ps1') -DatabaseFile (Join-Path $tmp 'regen.bin') -EngineExe $EngineExe 2>&1 | Select-Object -Last 1
        $fp = ($fpLine -split ' ')[0] -replace '^fingerprint=',''
        Check 'regenerated model fingerprint == pinned model' ($fp -eq $m.file.contentFingerprint) "found $fp"
    } finally {
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# ---------------------------------------------------------------------------
# LIVE MODE — a running engine must expose exactly the pinned datasets
# ---------------------------------------------------------------------------
if ($BaseUrl) {
    $base = $BaseUrl.TrimEnd('/')
    $headers = @{}
    if ($Token) { $headers['Authorization'] = "Bearer $Token" }
    try { $caps = Invoke-RestMethod "$base/capabilities" -TimeoutSec 5 } catch { Write-Host "SETUP: cannot reach $base/capabilities — $($_.Exception.Message)"; exit 2 }
    Check "protocolVersion == $($m.engine.protocolVersion)" ($caps.protocolVersion -eq [int]$m.engine.protocolVersion) "found $($caps.protocolVersion)"

    $datasets = @((Invoke-RestMethod "$base/datasets" -Headers $headers -TimeoutSec 5).datasets)
    $expected = @($m.datasets | ForEach-Object { $_.name } | Sort-Object)
    $actual = @($datasets | Where-Object { $_ -notlike '#*' } | Sort-Object)
    Check "user datasets == manifest" ((($expected -join '|') -eq ($actual -join '|'))) "expected [$($expected -join ', ')] found [$($actual -join ', ')]"

    if ($failures.Count -eq 0) {
        foreach ($ds in $m.datasets) {
            $desc = Invoke-RestMethod ("$base/" + [uri]::EscapeDataString($ds.name)) -Headers $headers -TimeoutSec 5
            $cols = @($desc.columns | ForEach-Object { "$($_.name):$($_.cardinality):$($_.suggestedKind)" }) -join '|'
            $want = @($ds.columns | ForEach-Object { "$($_.name):$($_.cardinality):$($_.kind)" }) -join '|'
            Check "$($ds.name): columns/cardinality/kinds" ($cols -eq $want) "expected [$want] found [$cols]"
            Check "$($ds.name): rowCount == $($ds.rowCount)" ($desc.rowCount -eq [int]$ds.rowCount) "found $($desc.rowCount)"
        }
        # Behavioural checks: the documented Sales totals.
        $body = '{"dataset":"Sales","groupBy":[{"name":"Region"}],"measures":[{"name":"value"}]}'
        $q = Invoke-RestMethod "$base/Sales/query" -Method Post -ContentType 'application/json; charset=utf-8' -Body ([System.Text.Encoding]::UTF8.GetBytes($body)) -Headers $headers -TimeoutSec 5
        foreach ($r in $q.rows) {
            $want = $m.checks.salesByRegion.($r[0])
            Check "Sales by Region: $($r[0])" ((CloseEnough $r[1] $want)) "expected $want found $($r[1])"
        }
        $grand = Invoke-RestMethod "$base/Sales/query" -Method Post -ContentType 'application/json; charset=utf-8' -Body ([System.Text.Encoding]::UTF8.GetBytes('{"dataset":"Sales","measures":[{"name":"value"}]}')) -Headers $headers -TimeoutSec 5
        Check "Sales grand total" ((CloseEnough $grand.rows[0][0] $m.checks.salesGrandTotal)) "expected $($m.checks.salesGrandTotal) found $($grand.rows[0][0])"
    }
}

Write-Host ""
if ($failures.Count -gt 0) {
    Write-Host "  RESULT: MISMATCH ($($failures.Count) failures) — this is NOT the pinned test model." -ForegroundColor Red
    exit 1
}
Write-Host "  RESULT: OK — matches $($m.modelName) $($m.modelVersion)." -ForegroundColor Green
exit 0
