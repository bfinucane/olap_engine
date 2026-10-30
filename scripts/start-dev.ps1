<#
  DEVELOPMENT engine launcher (D13/D14 isolation shape).

  This is the engine session's own instance:
    * builds `serve` into a SEPARATE target dir (workspace\olap-target-dev), so the frozen
      shared binary is never locked or replaced;
    * runs on a SEPARATE port (8090 by default);
    * uses a SCRATCH database copy (workspace\dev-work\database.bin) copied from the frozen
      model when available (else from the engine's database.bin), so it cannot mutate the
      agreed model or the shared instance;
    * uses its own security.json (development admin printed once on first run).

  USAGE
      .\start-engine-dev.cmd                 # build + start on 8090
      .\start-engine-dev.cmd -NoBuild        # use the existing dev build
      .\start-engine-dev.cmd -Fresh          # reset the scratch DB from the frozen model
      .\start-engine-dev.cmd -Cors           # browser client on another origin
      .\start-engine-dev.cmd -Anonymous      # loud, development-only

  Never expose this instance; it is not the shared server.
#>
param(
  [int]    $Port    = 8090,
  [string] $Bind    = '127.0.0.1',
  [switch] $Build,
  [switch] $NoBuild,
  [switch] $Fresh,
  [switch] $Cors,
  [switch] $Anonymous,
  [switch] $NoStart
)

$ErrorActionPreference = 'Stop'
$EngineDir  = Split-Path -Parent $PSScriptRoot
$Workspace  = Split-Path -Parent (Split-Path -Parent $EngineDir)
$TargetDir  = Join-Path $Workspace 'olap-target-dev'
$DevWork    = Join-Path $Workspace 'dev-work'
$DevDb      = Join-Path $DevWork 'database.bin'
$DevSec     = Join-Path $DevWork 'security.json'
$FrozenDb   = Join-Path $Workspace 'shared\database.bin'
$FrozenExe  = Join-Path $Workspace 'shared\serve.exe'
$DevExe     = Join-Path $TargetDir 'debug\serve.exe'

. (Join-Path $PSScriptRoot 'lib-bootstrap.ps1')

Write-Host ""
Write-Host "  DEVELOPMENT engine (isolated build + scratch DB; NOT the shared server)" -ForegroundColor Cyan
Write-Host ""

if (-not $NoBuild) {
  if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "  ERROR: cargo is not on PATH." -ForegroundColor Red
    Write-Host "  Build manually:  cd `"$EngineDir`"; `$env:CARGO_TARGET_DIR='$TargetDir'; cargo build --offline --bin serve"
    exit 1
  }
  Write-Host "  Building serve.exe into $TargetDir ..."
  Push-Location $EngineDir
  try {
    $env:CARGO_TARGET_DIR = $TargetDir
    & cargo build --offline --bin serve
    $buildExit = $LASTEXITCODE
  } finally {
    Pop-Location
    Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
  }
  if ($buildExit -ne 0) {
    Write-Host "  ERROR: build failed (exit $buildExit)." -ForegroundColor Red
    exit 1
  }
  Write-Host "  Build OK."
}

if (-not (Test-Path $DevExe)) {
  Write-Host "  ERROR: no dev binary at $DevExe" -ForegroundColor Red
  Write-Host "  Run without -NoBuild to build it."
  exit 1
}

Get-FreePortOrFail -Port $Port -Launcher '.\start-engine-dev.cmd' -Alternative '8091'

if ($Fresh -or -not (Test-Path $DevDb)) {
  New-Item -ItemType Directory -Force -Path $DevWork | Out-Null
  $source = if (Test-Path $FrozenDb) { $FrozenDb } else { Join-Path $EngineDir 'database.bin' }
  Copy-Item $source $DevDb -Force
  Write-Host "  scratch DB reset from: $source"
}

if (-not $NoStart -and -not $Anonymous) {
  $created = New-DevelopmentAdmin -Server $DevExe -SecurityFile $DevSec
  if ($created) {
    Write-Host "  The credentials above are shown ONCE. Press Enter once you have stored them." -ForegroundColor Yellow
    Write-Host "  To replace a lost password, stop the server, delete dev-work\security.json, restart." -ForegroundColor Yellow
    [void](Read-Host)
  }
}

$args = @('--bind', "${Bind}:${Port}", '--db', $DevDb, '--security', $DevSec)
if ($Anonymous) { $args += '--allow-anonymous' }
if ($Cors)      { $args += '--cors' }

Write-Host ""
Show-ArtifactManifest -ServerExe $DevExe -DatabaseFile $DevDb
Write-Host "  source commit ->  $(& git -C $EngineDir log -1 --format='%h %s' 2>$null)"
Write-Host "  address       ->  http://${Bind}:${Port}"
Write-Host "  scratch       ->  dev-work\database.bin (never the frozen model)"
if ($Anonymous) {
  Write-Host ""
  Write-Host "  ############################################################" -ForegroundColor Red
  Write-Host "  #  AUTH DISABLED - DEVELOPMENT ONLY                         #" -ForegroundColor Red
  Write-Host "  #  Never expose this server.                                 #" -ForegroundColor Red
  Write-Host "  ###########################################################" -ForegroundColor Red
}
Write-Host ""
Write-Host "  Ctrl+C to stop. This instance is not the shared server."
Write-Host ""

if ($NoStart) { Write-Host "  (NoStart: validated the dev instance only; nothing was started.)"; exit 0 }
& $DevExe @args
