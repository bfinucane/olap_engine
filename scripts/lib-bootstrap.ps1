# Bootstrap helper shared by the launchers.
#
# Contract handoff 007 / ruling D12: a first run with no security.json must not be a dead
# end. For DEVELOPMENT launchers only: create one admin account with a random password,
# print the credentials ONCE, and keep the loud "development-only" banner. Never silently
# anonymous; never overwrite an existing security file.

function New-DevelopmentAdmin {
    param(
        [Parameter(Mandatory=$true)][string]$Server,
        [Parameter(Mandatory=$true)][string]$SecurityFile
    )
    if (Test-Path $SecurityFile) { return $false }

    $bytes = New-Object byte[] 24
    $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    try { $rng.GetBytes($bytes) } finally { $rng.Dispose() }
    $password = [Convert]::ToBase64String($bytes).TrimEnd('=').Replace('+', '-').Replace('/', '_')

    Write-Host ""
    Write-Host "  ============================================================" -ForegroundColor Yellow
    Write-Host "   FIRST RUN: creating a DEVELOPMENT-ONLY admin account" -ForegroundColor Yellow
    Write-Host "   (auth stays REQUIRED; this is not an anonymous mode)" -ForegroundColor Yellow
    Write-Host "  ============================================================" -ForegroundColor Yellow
    & $Server --add-user admin --role admin --password $password --security $SecurityFile
    if ($LASTEXITCODE -ne 0) {
        Write-Host "  ERROR: could not create the development admin (exit $LASTEXITCODE)." -ForegroundColor Red
        exit 1
    }
    Write-Host ""
    Write-Host "   DEVELOPMENT CREDENTIALS (shown ONCE - store them now)" -ForegroundColor Yellow
    Write-Host "     user      admin"
    Write-Host "     password  $password"
    Write-Host "     file      $SecurityFile"
    Write-Host ""
    Write-Host "   Development only. For a real deployment:" -ForegroundColor Yellow
    Write-Host "     serve.exe --add-user <name> --role <role> --security `"$SecurityFile`""
    Write-Host ""
    return $true
}

function Get-FreePortOrFail {
    param([int]$Port, [string]$Launcher, [string]$Alternative)
    $inUse = $null
    try { $inUse = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction Stop } catch { }
    if ($inUse) {
        Write-Host "  ERROR: port $Port is already in use (pid $($inUse.OwningProcess))." -ForegroundColor Red
        Write-Host "  Stop that process, or use another port:  $Launcher -Port $Alternative"
        exit 1
    }
}

function Get-ShortHash {
    param([string]$Path)
    return (Get-FileHash $Path -Algorithm SHA256).Hash
}

function Show-ArtifactManifest {
    param([string]$ServerExe, [string]$DatabaseFile)
    Write-Host "  server       ->  $ServerExe"
    Write-Host "  model        ->  $DatabaseFile"
    Write-Host "  server sha256  $((Get-ShortHash $ServerExe))"
    Write-Host "  model  sha256  $((Get-ShortHash $DatabaseFile))"
}
