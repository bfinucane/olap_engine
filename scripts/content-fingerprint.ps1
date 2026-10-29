<#
  Content fingerprint of a database file (operational shape, not bytes).

  WHY: database saves are NOT byte-reproducible - bincode serialises HashMap-backed
  catalogues in iteration order, so two clean seed runs produce the same model with
  different SHA-256 file hashes. A model pin therefore needs BOTH:
    * the exact frozen file SHA-256 (what the shared server actually loads), and
    * this content fingerprint, which is stable across regenerations.

  It runs a fixed REPL probe against a TEMPORARY COPY (never the input file: the REPL saves
  on exit), normalises away banner/path/timing noise, and hashes the semantic lines. The
  probe covers cube list, dimensions, every user-cube schema, and two Sales aggregate
  queries (by Region; by Region+Segment).

  USAGE (from the engine repo root or anywhere):
      .\scripts\content-fingerprint.ps1 -DatabaseFile <path>
      .\scripts\content-fingerprint.ps1 -DatabaseFile database.bin -EngineExe target\debug\olap_engine.exe

  Exit code 0 and one line on stdout:
      fingerprint=<sha256> lines=<n> size=<bytes> file_sha256=<sha256>
#>
param(
  [Parameter(Mandatory=$true)][string]$DatabaseFile,
  [string]$EngineExe
)

$ErrorActionPreference = 'Stop'
$EngineDir = Split-Path -Parent $PSScriptRoot
if (-not $EngineExe) { $EngineExe = Join-Path $EngineDir 'target\debug\olap_engine.exe' }
if (-not (Test-Path $DatabaseFile)) { throw "no such database file: $DatabaseFile" }
if (-not (Test-Path $EngineExe))   { throw "no engine binary: $EngineExe" }

$script = @(
  '.cubes', '.dimensions',
  '.schema Sales', '.schema Pivot', '.schema CostCentre', '.schema Geography',
  '.schema ProductClass', '.schema Attributes', '.schema RegionAttr',
  'SELECT Region, SUM(value) FROM Sales;',
  'SELECT Region, Segment, SUM(value) FROM Sales;',
  '.exit'
)

$tmp = Join-Path $env:TEMP ("olap-fingerprint-" + [guid]::NewGuid().ToString('N') + ".bin")
try {
  Copy-Item $DatabaseFile $tmp -Force
  $raw = $script | & $EngineExe --db $tmp 2>&1
  $lines = @($raw | Where-Object {
      $_ -notmatch 'Patria OLAP Engine v|=====|Database file:|Loading existing|Type \.help|Saving database|Goodbye|^\s*$|\(Time:'
    } | ForEach-Object { $_.Trim() })
  $text = ($lines -join "`n")
  $fp = (Get-FileHash -InputStream ([System.IO.MemoryStream]::new([System.Text.Encoding]::UTF8.GetBytes($text))) -Algorithm SHA256).Hash
  $fileSha = (Get-FileHash $DatabaseFile -Algorithm SHA256).Hash
  $size = (Get-Item $DatabaseFile).Length
  Write-Output "fingerprint=$fp lines=$($lines.Count) size=$size file_sha256=$fileSha"
} finally {
  Remove-Item $tmp -Force -ErrorAction SilentlyContinue
}
