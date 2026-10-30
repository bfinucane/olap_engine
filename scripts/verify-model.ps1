<#
  verify-model.ps1 — assert that a database file, or a running engine, IS the pinned model.

  Pin: model-manifest.v2.json (v1 kept as history). The pin has two halves because a seed
  regeneration is NOT byte-reproducible (bincode walks HashMap-backed catalogues in iteration
  order):
    * exact frozen artefact  = file size + SHA-256
    * model content         = SHA-256 over canonical HTTP evidence (datasets, every describe,
                              measure totals, totals by Region, line-item count)
  plus the manifest checks themselves.

  MODES
      .\scripts\verify-model.ps1 -DatabaseFile shared\database.bin      # file: size+SHA+fingerprint
      .\scripts\verify-model.ps1 -DatabaseFile x.bin -Regenerate        # seed + fingerprint + checks
      .\scripts\verify-model.ps1 -BaseUrl http://127.0.0.1:8080/v1 [-Token <bearer>]
      .\scripts\verify-model.ps1 -DatabaseFile x.bin -PrintFingerprint  # fingerprint only

  Exit: 0 = matches; 1 = mismatch; 2 = setup failure.
#>
param(
  [string] $DatabaseFile,
  [string] $BaseUrl,
  [string] $Token,
  [string] $EngineExe,
  [string] $ServeExe,
  [string] $Manifest = 'model-manifest.v2.json',
  [switch] $Regenerate,
  [switch] $PrintFingerprint
)
$ErrorActionPreference = 'Stop'
$EngineDir = Split-Path -Parent $PSScriptRoot
if (-not $EngineExe) { $EngineExe = Join-Path $EngineDir 'target\debug\olap_engine.exe' }
if (-not $ServeExe)  { $ServeExe  = Join-Path $EngineDir 'target\debug\serve.exe' }
$ManifestPath = Join-Path $EngineDir $Manifest
if (-not (Test-Path $ManifestPath)) { Write-Host "SETUP: no manifest at $ManifestPath"; exit 2 }
$m = Get-Content $ManifestPath -Raw | ConvertFrom-Json
$failures = New-Object 'System.Collections.Generic.List[string]'
function Check([string]$label, [bool]$ok, [string]$detail) {
    if ($ok) { Write-Host "  PASS $label" }
    else { Write-Host "  FAIL $label : $detail"; [void]$failures.Add("$label : $detail") }
}
function CloseEnough($a, $b) { return [Math]::Abs([double]$a - [double]$b) -le 0.005 }
function Sha256Hex([string]$text) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { return ([System.BitConverter]::ToString($sha.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($text)))).Replace('-','') }
    finally { $sha.Dispose() }
}
function Format2($v) { return ([Math]::Round([double]$v, 2)).ToString('F2', [System.Globalization.CultureInfo]::InvariantCulture) }
function Get-FreePort {
    for ($i = 0; $i -lt 20; $i++) {
        $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
        $listener.Start()
        try { return ([System.Net.IPEndPoint]$listener.LocalEndpoint).Port } finally { $listener.Stop() }
    }
    throw 'no free port'
}
function Query([string]$base, [hashtable]$headers, [string]$path, [string]$json) {
    $params = @{ Uri = "$base$path"; Method = 'Post'; ContentType = 'application/json; charset=utf-8'; Body = [System.Text.Encoding]::UTF8.GetBytes($json); TimeoutSec = 60 }
    if ($headers.Count -gt 0) { $params.Headers = $headers }
    return Invoke-RestMethod @params
}
function Start-TempEngine([string]$dbPath) {
    if (-not (Test-Path $ServeExe)) { throw "no serve binary at $ServeExe" }
    $port = Get-FreePort
    $stdout = [System.IO.Path]::GetTempFileName(); $stderr = [System.IO.Path]::GetTempFileName()
    $proc = Start-Process $ServeExe -ArgumentList @('--bind', "127.0.0.1:$port", '--db', $dbPath, '--allow-anonymous') -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    try {
        $base = "http://127.0.0.1:$port/v1"
        for ($i = 0; $i -lt 300; $i++) {
            try { $null = Invoke-RestMethod "$base/capabilities" -TimeoutSec 2; return @{ Proc = $proc; Base = $base } }
            catch { Start-Sleep -Milliseconds 200 }
        }
        throw "engine on $base did not start"
    } catch { try { Stop-Process -Id $proc.Id -Force } catch { }; throw }
}
function Get-Evidence([string]$base, [hashtable]$headers) {
    $lines = New-Object 'System.Collections.Generic.List[string]'
    $datasets = @((Invoke-RestMethod "$base/datasets" -Headers $headers -TimeoutSec 60).datasets | Where-Object { $_ -notlike '#*' } | Sort-Object)
    $lines.Add('datasets=' + ($datasets -join ','))
    foreach ($ds in $m.datasets) {
        $desc = Invoke-RestMethod ("$base/" + [uri]::EscapeDataString($ds.name)) -Headers $headers -TimeoutSec 60
        $cols = @($desc.columns | ForEach-Object { "$($_.name):$($_.suggestedKind):$($_.cardinality)" }) -join '|'
        $lines.Add("dataset=$($ds.name)|rowCount=$($desc.rowCount)|columns=$cols")
    }
    $names = @($m.checks.totals.PSObject.Properties.Name)
    $mj = ($names | ForEach-Object { '{"name":"' + $_ + '"}' }) -join ','
    $q = Query $base $headers ("/" + $m.datasets[0].name + "/query") ('{"dataset":"' + $m.datasets[0].name + '","measures":[' + $mj + ']}')
    $returned = @($q.measureNames); $vals = @{}
    for ($i = 0; $i -lt $returned.Count; $i++) { $vals[$returned[$i]] = $q.rows[0][$i] }
    $lines.Add('totals=' + ((@($names | Sort-Object | ForEach-Object { "$($_)=$(Format2 $vals[$_])" })) -join '|'))
    $region = @($m.checks.byRegion.PSObject.Properties | Select-Object -First 1).Value
    $rm = @($region.PSObject.Properties.Name)
    $rmj = ($rm | ForEach-Object { '{"name":"' + $_ + '"}' }) -join ','
    $rq = Query $base $headers ("/" + $m.datasets[0].name + "/query") ('{"dataset":"' + $m.datasets[0].name + '","groupBy":[{"name":"Region"}],"measures":[' + $rmj + ']}')
    $rows = @($rq.rows | ForEach-Object { $row = $_; $pairs = @($rm | ForEach-Object { $i = [array]::IndexOf($rm, $_) + 1; "$($_)=$(Format2 $row[$i])" }) -join '|'; "$($row[0])|$pairs" } | Sort-Object) -join ';'
    $lines.Add('byRegion=' + $rows)
    $dims = @($m.datasets[0].columns | Where-Object { $_.kind -eq 'dimension' } | ForEach-Object { $_.name })
    $gb = ($dims | ForEach-Object { '{"name":"' + $_ + '"}' }) -join ','
    $fj = '{"field":"' + $m.checks.lineItems.filterField + '","operator":"in","values":["' + $m.checks.lineItems.filterValue + '"]}'
    $lq = '{"dataset":"' + $m.datasets[0].name + '","groupBy":[' + $gb + '],"measures":[],"filters":[' + $fj + ']}'
    $lines.Add('lineItems=' + (@((Query $base $headers ("/" + $m.datasets[0].name + "/query") $lq).rows).Count))
    return ($lines -join "`n")
}
function FingerprintForFile([string]$dbPath) {
    $server = Start-TempEngine $dbPath
    try { return (Sha256Hex (Get-Evidence $server.Base @{})) }
    finally { try { Stop-Process -Id $server.Proc.Id -Force } catch { } }
}
function Report-Checks([string]$base, [hashtable]$headers) {
    $datasets = @((Invoke-RestMethod "$base/datasets" -Headers $headers -TimeoutSec 60).datasets | Where-Object { $_ -notlike '#*' } | Sort-Object)
    $expected = @($m.datasets | ForEach-Object { $_.name } | Sort-Object)
    Check 'datasets == manifest' (($datasets -join '|') -eq ($expected -join '|')) "expected [$($expected -join ', ')] found [$($datasets -join ', ')]"
    foreach ($ds in $m.datasets) {
        $desc = Invoke-RestMethod ("$base/" + [uri]::EscapeDataString($ds.name)) -Headers $headers -TimeoutSec 60
        $cols = @($desc.columns | ForEach-Object { "$($_.name):$($_.suggestedKind):$($_.cardinality)" }) -join '|'
        $want = @($ds.columns | ForEach-Object { "$($_.name):$($_.kind):$($_.cardinality)" }) -join '|'
        Check "$($ds.name): columns/cardinality/kinds" ($cols -eq $want) "expected [$want] found [$cols]"
        Check "$($ds.name): rowCount == $($ds.rowCount)" ($desc.rowCount -eq [int]$ds.rowCount) "found $($desc.rowCount)"
    }
    $names = @($m.checks.totals.PSObject.Properties.Name)
    $mj = ($names | ForEach-Object { '{"name":"' + $_ + '"}' }) -join ','
    $q = Query $base $headers ("/" + $m.datasets[0].name + "/query") ('{"dataset":"' + $m.datasets[0].name + '","measures":[' + $mj + ']}')
    $returned = @($q.measureNames); $vals = @{}
    for ($i = 0; $i -lt $returned.Count; $i++) { $vals[$returned[$i]] = $q.rows[0][$i] }
    foreach ($measure in $names) { Check "total $measure == $($m.checks.totals.$measure)" (CloseEnough $vals[$measure] $m.checks.totals.$measure) "found $($vals[$measure])" }
    $region = @($m.checks.byRegion.PSObject.Properties | Select-Object -First 1).Value
    $rm = @($region.PSObject.Properties.Name)
    $rmj = ($rm | ForEach-Object { '{"name":"' + $_ + '"}' }) -join ','
    $rq = Query $base $headers ("/" + $m.datasets[0].name + "/query") ('{"dataset":"' + $m.datasets[0].name + '","groupBy":[{"name":"Region"}],"measures":[' + $rmj + ']}')
    foreach ($row in $rq.rows) {
        foreach ($measure in $rm) {
            $idx = [array]::IndexOf($rm, $measure) + 1
            $want = $m.checks.byRegion.($row[0]).$measure
            Check "Region $($row[0]) $measure == $want" (CloseEnough $row[$idx] $want) "found $($row[$idx])"
        }
    }
    $dims = @($m.datasets[0].columns | Where-Object { $_.kind -eq 'dimension' } | ForEach-Object { $_.name })
    $gb = ($dims | ForEach-Object { '{"name":"' + $_ + '"}' }) -join ','
    $fj = '{"field":"' + $m.checks.lineItems.filterField + '","operator":"in","values":["' + $m.checks.lineItems.filterValue + '"]}'
    $lq = '{"dataset":"' + $m.datasets[0].name + '","groupBy":[' + $gb + '],"measures":[],"filters":[' + $fj + ']}'
    $count = @((Query $base $headers ("/" + $m.datasets[0].name + "/query") $lq).rows).Count
    Check "line items == $($m.checks.lineItems.expected)" ($count -eq [int]$m.checks.lineItems.expected) "found $count"
}
Write-Host ""
Write-Host "  model: $($m.modelName) $($m.modelVersion)  (engine $($m.engine.packageVersion), protocol $($m.engine.protocolVersion), contract $($m.contract))"
Write-Host ""
if ($PrintFingerprint) {
    if (-not $DatabaseFile -or -not (Test-Path $DatabaseFile)) { Write-Host 'SETUP: -PrintFingerprint needs -DatabaseFile'; exit 2 }
    Write-Output (FingerprintForFile (Resolve-Path $DatabaseFile).Path)
    exit 0
}
if ($DatabaseFile) {
    if (-not (Test-Path $DatabaseFile)) { Write-Host "SETUP: no such database file: $DatabaseFile"; exit 2 }
    Check "file size == $($m.file.size) B" (((Get-Item $DatabaseFile).Length) -eq [int]$m.file.size) "found $((Get-Item $DatabaseFile).Length) B"
    Check 'file SHA-256 == pinned release' (((Get-FileHash $DatabaseFile -Algorithm SHA256).Hash) -eq $m.file.sha256) "found $((Get-FileHash $DatabaseFile -Algorithm SHA256).Hash)"
    $fp = FingerprintForFile (Resolve-Path $DatabaseFile).Path
    Check 'content fingerprint == pinned model' ($fp -eq $m.file.contentFingerprint) "found $fp"
}
if ($Regenerate) {
    $tmp = Join-Path $env:TEMP ('olap-regen-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    try {
        Push-Location $EngineDir
        try { & $EngineExe --db (Join-Path $tmp 'regen.bin') $m.seed *> (Join-Path $tmp 'seed.log') } finally { Pop-Location }
        Check 'seed regeneration exits 0' ($LASTEXITCODE -eq 0) "exit $LASTEXITCODE (see $tmp\seed.log)"
        $fp = FingerprintForFile (Join-Path $tmp 'regen.bin')
        Check 'regenerated model fingerprint == pinned model' ($fp -eq $m.file.contentFingerprint) "found $fp"
        $server = Start-TempEngine (Join-Path $tmp 'regen.bin')
        try { Report-Checks $server.Base @{} } finally { try { Stop-Process -Id $server.Proc.Id -Force } catch { } }
    } finally { Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue }
}
if ($BaseUrl) {
    $base = $BaseUrl.TrimEnd('/'); $headers = @{}
    if ($Token) { $headers['Authorization'] = "Bearer $Token" }
    try { $null = Invoke-RestMethod "$base/capabilities" -TimeoutSec 10 } catch { Write-Host "SETUP: cannot reach $base/capabilities : $($_.Exception.Message)"; exit 2 }
    Check 'content fingerprint == pinned model' ((Sha256Hex (Get-Evidence $base $headers)) -eq $m.file.contentFingerprint) 'evidence differs from the pinned model'
    Report-Checks $base $headers
}
Write-Host ""
if ($failures.Count -gt 0) { Write-Host "  RESULT: MISMATCH ($($failures.Count) failures) — NOT the pinned test model." -ForegroundColor Red; exit 1 }
Write-Host "  RESULT: OK — matches $($m.modelName) $($m.modelVersion)." -ForegroundColor Green
exit 0