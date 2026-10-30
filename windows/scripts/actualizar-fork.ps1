[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$logDir = Join-Path $env:LOCALAPPDATA 'Coucou'
$logPath = Join-Path $logDir 'actualizar-fork.log'

function Write-UpdateLog {
    param([Parameter(Mandatory)][string]$Message)
    New-Item -ItemType Directory -Force -Path $logDir | Out-Null
    $stamp = Get-Date -Format 'yyyy-MM-dd HH:mm:ss'
    Add-Content -LiteralPath $logPath -Value "[$stamp] $Message" -Encoding utf8
}

function Invoke-Git {
    param([Parameter(Mandatory)][string[]]$Arguments)
    & git.exe -C $repoRoot @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "git $($Arguments -join ' ') terminó con código $LASTEXITCODE"
    }
}

try {
    Invoke-Git -Arguments @('fetch', 'upstream')
    Invoke-Git -Arguments @('switch', 'barra-tareas')

    $before = (& git.exe -C $repoRoot rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'No se pudo leer el commit actual.' }

    $rebaseOutput = (& git.exe -C $repoRoot rebase upstream/main 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {
        $rebaseMessage = "El rebase sobre upstream/main falló; se ha abortado sin cambiar la rama. Motivo: $rebaseOutput"
        & git.exe -C $repoRoot rebase --abort | Out-Null
        Write-UpdateLog $rebaseMessage
        exit 2
    }

    $after = (& git.exe -C $repoRoot rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'No se pudo leer el commit después del rebase.' }
    if ($before -eq $after) {
        exit 0
    }

    $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path -LiteralPath $cargoBin) {
        $env:Path = "$cargoBin;$env:Path"
    }

    $windowsDir = Join-Path $repoRoot 'windows'
    & npm.cmd run pack --prefix $windowsDir
    if ($LASTEXITCODE -ne 0) {
        throw "npm run pack terminó con código $LASTEXITCODE"
    }

    $releaseDir = Join-Path $windowsDir 'release'
    $installer = Get-ChildItem -LiteralPath $releaseDir -Filter 'Coucou-Windows-*-setup.exe' -File |
        Where-Object { $_.Name -ne 'Coucou-Windows-setup.exe' } |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -First 1
    if (-not $installer) {
        throw "No se encontró el instalador versionado en $releaseDir"
    }

    Get-Process -Name 'coucou' -ErrorAction SilentlyContinue | Stop-Process -Force
    $install = Start-Process -FilePath $installer.FullName -ArgumentList @('/S', '/UPDATE') -Wait -PassThru
    if ($install.ExitCode -ne 0) {
        throw "El instalador terminó con código $($install.ExitCode)"
    }

    $installedExe = Join-Path $env:LOCALAPPDATA 'Coucou\coucou.exe'
    if (-not (Test-Path -LiteralPath $installedExe)) {
        throw "No se encontró la aplicación instalada en $installedExe"
    }
    Start-Process -FilePath $installedExe
    Write-UpdateLog "Actualizado $before -> $after e instalado $($installer.Name)."

    # The rebase rewrote the branch: the lease refuses to overwrite commits pushed from elsewhere.
    $pushOutput = (& git.exe -C $repoRoot push --force-with-lease origin barra-tareas 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {
        Write-UpdateLog "AVISO: no se pudo subir barra-tareas a origin: $pushOutput"
    }
    exit 0
}
catch {
    Write-UpdateLog "ERROR: $($_.Exception.Message)"
    exit 1
}
