<#
  Promote a release into the frozen shared pair (contract handoff 009 / ruling D14).

  Run this as the USER, with the shared server STOPPED. It copies the candidate from
  shared\candidate\ to shared\, keeps the previous release under shared\previous\<timestamp>\,
  and prints the full SHA-256 values that must be recorded in contract/SHARED-ENGINE.md
  before the restart broadcast goes out.

  The engine session prepares candidates; it never promotes or restarts the shared instance.

  USAGE
      .\Patricia\olap_engine\scripts\promote-release.ps1
      .\Patricia\olap_engine\scripts\promote-release.ps1 -Candidate shared\candidate
#>
param(
  [string] $SharedDir,
  [string] $CandidateDir,
  [int]    $Port = 8080
)

$ErrorActionPreference = 'Stop'
$EngineDir = Split-Path -Parent $PSScriptRoot
$Workspace = Split-Path -Parent (Split-Path -Parent $EngineDir)
if (-not $SharedDir)    { $SharedDir = Join-Path $Workspace 'shared' }
if (-not $CandidateDir) { $CandidateDir = Join-Path $SharedDir 'candidate' }

$CandExe = Join-Path $CandidateDir 'serve.exe'
$CandDb  = Join-Path $CandidateDir 'database.bin'

Write-Host ""
Write-Host "  PROMOTE RELEASE -> $SharedDir" -ForegroundColor Cyan
Write-Host ""

foreach ($f in @($CandExe, $CandDb)) {
  if (-not (Test-Path $f)) {
    Write-Host "  ERROR: missing candidate artefact: $f" -ForegroundColor Red
    exit 1
  }
}

# RELEASE HYGIENE (decided 2026-11-02): once the maintenance command exists
# (scripts\maintain-db.cmd --db <candidate> --steps all --backup, followed by a
# verify-model fingerprint check), this script must run it on the CANDIDATE before
# hashing and swapping, and abort on failure. It is not called yet: maintenance ships
# with the fact-set storage work, and promotion must not silently claim hygiene it did
# not perform.
# Refuse to swap while anything is listening on the shared port.
$inUse = $null
try { $inUse = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction Stop } catch { }
if ($inUse) {
  Write-Host "  ERROR: port $Port is in use (pid $($inUse.OwningProcess)); stop the shared server first." -ForegroundColor Red
  exit 1
}

$stamp = Get-Date -Format 'yyyy-MM-dd-HHmmss'
$previous = Join-Path $SharedDir "previous\$stamp"
foreach ($f in @('serve.exe', 'database.bin')) {
  $p = Join-Path $SharedDir $f
  if (Test-Path $p) {
    New-Item -ItemType Directory -Force -Path $previous | Out-Null
    Copy-Item $p (Join-Path $previous $f) -Force
  }
}
New-Item -ItemType Directory -Force -Path $SharedDir | Out-Null
Copy-Item $CandExe (Join-Path $SharedDir 'serve.exe') -Force
Copy-Item $CandDb  (Join-Path $SharedDir 'database.bin') -Force

# The running copy is derived state: never let a promotion inherit it.
$running = Join-Path $SharedDir 'running\database.bin'
if (Test-Path $running) { Remove-Item $running -Force }

Write-Host "  promoted. Record these in contract/SHARED-ENGINE.md BEFORE the restart:" -ForegroundColor Green
Write-Host ""
Write-Host "    serve.exe     sha256 = $((Get-FileHash (Join-Path $SharedDir 'serve.exe') -Algorithm SHA256).Hash)"
Write-Host "    database.bin  sha256 = $((Get-FileHash (Join-Path $SharedDir 'database.bin') -Algorithm SHA256).Hash)"
Write-Host "    engine commit         = $(git -C $EngineDir log -1 --format='%h %s' 2>$null)"
Write-Host "    promoted at           = $(Get-Date -Format 'yyyy-MM-ddTHH:mm:ssK')"
if (Test-Path $previous) { Write-Host "    previous release kept = $previous" }
Write-Host ""
Write-Host "  Then: start the shared server (.\start-engine.cmd) and BROADCAST to all clients:" -ForegroundColor Yellow
Write-Host "    engine commit, contract version, port, auth mode, model sha256, restart time/by/reason;"
Write-Host "    clients re-login (tokens reset) and must have saved before the restart."
Write-Host ""
Write-Host "  Do not forget to unverified/verify the model hash with the owner (D14 §3)."
