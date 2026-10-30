<#
  SHARED engine launcher (contract handoff 009 / ruling D14).

  Serves the FROZEN pair and never builds:
      shared\serve.exe      <- promoted from a named engine commit
      shared\database.bin   <- the named model (content-fingerprinted)

  Only the user promotes a release (scripts\promote-release.ps1: stop -> swap -> record
  hashes -> start -> update contract/SHARED-ENGINE.md -> broadcast). This launcher refuses
  to start until the pair exists; it never falls back to the development tree.

  At every start it copies the frozen model + security file into shared\running\ and serves
  THOSE:
    * the frozen model is never mutated (a POST /v1/save writes the running copy);
    * a restart always comes back to the frozen model, so the agreed model is stable;
    * auth tokens live in memory, so a restart logs every client out (re-login);
    * unsaved writes in the running copy are lost on restart - save first if you need them.

  USAGE (workspace root wrappers call this):
      .\start-engine.cmd                  # port 8080, frozen pair
      .\start-engine.cmd -Fresh           # reset shared\running from the frozen model
      .\start-engine.cmd -Anonymous       # DEVELOPMENT ONLY banner; never the shared default

  Run it in the USER's terminal. Do not run it as a session job.
#>
param(
  [int]    $Port    = 8080,
  [string] $Bind    = '127.0.0.1',
  [switch] $Fresh,
  [switch] $Cors,
  [switch] $Anonymous,
  [switch] $NoStart,
  [string] $SharedDir
)

$ErrorActionPreference = 'Stop'
$EngineDir   = Split-Path -Parent $PSScriptRoot
$Workspace   = Split-Path -Parent (Split-Path -Parent $EngineDir)
if (-not $SharedDir) { $SharedDir = Join-Path $Workspace 'shared' }
$SharedExe   = Join-Path $SharedDir 'serve.exe'
$SharedDb    = Join-Path $SharedDir 'database.bin'
$RunningDir  = Join-Path $SharedDir 'running'
$RunningDb   = Join-Path $RunningDir 'database.bin'
$RunningSec  = Join-Path $RunningDir 'security.json'

. (Join-Path $PSScriptRoot 'lib-bootstrap.ps1')

Write-Host ""
Write-Host "  SHARED engine (frozen release; no build)" -ForegroundColor Cyan
Write-Host ""

if (-not (Test-Path $SharedExe) -or -not (Test-Path $SharedDb)) {
  Write-Host "  ERROR: no frozen release at $SharedDir" -ForegroundColor Red
  Write-Host "  The shared launcher never falls back to the development tree." -ForegroundColor Red
  Write-Host ""
  Write-Host "  Prepare a candidate (engine session does this):"
  Write-Host "      shared\candidate\serve.exe + shared\candidate\database.bin"
  Write-Host "  Promote it (the USER does this, with the server stopped):"
  Write-Host "      .\Patricia\olap_engine\scripts\promote-release.ps1"
  Write-Host ""
  Write-Host "  Development builds run elsewhere:  .\start-engine-dev.cmd"
  exit 1
}

Get-FreePortOrFail -Port $Port -Launcher '.\start-engine.cmd' -Alternative '8090 (but use start-engine-dev.cmd for dev builds)'

if ($Fresh -or -not (Test-Path $RunningDb)) {
  New-Item -ItemType Directory -Force -Path $RunningDir | Out-Null
  Copy-Item $SharedDb $RunningDb -Force
  Write-Host "  reset shared\running\database.bin from the frozen model"
}

if (-not $NoStart -and -not $Anonymous) {
  $created = New-DevelopmentAdmin -Server $SharedExe -SecurityFile $RunningSec
  if ($created) {
    Write-Host "  The credentials above are shown ONCE. Press Enter once you have stored them;" -ForegroundColor Yellow
    Write-Host "  the window will then start the server and stay open. To replace a lost password," -ForegroundColor Yellow
    Write-Host "  stop the server, delete shared\running\security.json, and start again." -ForegroundColor Yellow
    [void](Read-Host)
  }
}

$args = @('--bind', "${Bind}:${Port}", '--db', $RunningDb, '--security', $RunningSec)
if ($Anonymous) { $args += '--allow-anonymous' }
if ($Cors)      { $args += '--cors' }

Write-Host ""
Show-ArtifactManifest -ServerExe $SharedExe -DatabaseFile $SharedDb
Write-Host "  serving      ->  shared\running\database.bin (copy; frozen model untouched)"
Write-Host "  address      ->  http://${Bind}:${Port}"
if ($Anonymous) {
  Write-Host ""
  Write-Host "  ############################################################" -ForegroundColor Red
  Write-Host "  #  AUTH DISABLED - DEVELOPMENT ONLY (not the shared default) #" -ForegroundColor Red
  Write-Host "  ############################################################" -ForegroundColor Red
}
Write-Host ""
Write-Host "  Clients: credentials from the user (out-of-band). A restart logs everyone out;" -ForegroundColor Yellow
Write-Host "  unsaved writes in the running copy are lost. Only the user saves this instance." -ForegroundColor Yellow
Write-Host ""
Write-Host "  Ctrl+C to stop."
Write-Host ""

if ($NoStart) { Write-Host "  (NoStart: validated the frozen pair only; nothing was started.)"; exit 0 }
& $SharedExe @args
