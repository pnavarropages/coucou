[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$updateScript = (Resolve-Path (Join-Path $PSScriptRoot 'actualizar-fork.ps1')).Path
$taskName = 'Coucou - Actualizar fork'
$currentUser = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name

$action = New-ScheduledTaskAction `
    -Execute 'powershell.exe' `
    -Argument "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$updateScript`""
$triggers = @(
    New-ScheduledTaskTrigger -Daily -At '09:00'
    New-ScheduledTaskTrigger -AtLogOn -User $currentUser
)
$principal = New-ScheduledTaskPrincipal `
    -UserId $currentUser `
    -LogonType Interactive `
    -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet `
    -StartWhenAvailable `
    -MultipleInstances IgnoreNew `
    -ExecutionTimeLimit (New-TimeSpan -Hours 6)

Register-ScheduledTask `
    -TaskName $taskName `
    -Action $action `
    -Trigger $triggers `
    -Principal $principal `
    -Settings $settings `
    -Description 'Actualiza y recompila el fork de Coucou desde upstream sin sustituirlo por la versión oficial.' `
    -Force

Write-Host "Tarea '$taskName' registrada para las 09:00 y al iniciar sesión."
