param(
  [string]$InstallDir = "$env:ProgramFiles\Hotswitch"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$serviceName = 'Hotswitch'

function Assert-Admin {
  $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
  $principal = New-Object Security.Principal.WindowsPrincipal($identity)
  if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run uninstall-hotswitch.ps1 from an elevated PowerShell prompt.'
  }
}

Assert-Admin

$service = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if ($service) {
  if ($service.Status -ne 'Stopped') {
    sc.exe stop $serviceName | Out-Null
    Start-Sleep -Seconds 2
  }
  sc.exe delete $serviceName | Out-Null
}

schtasks.exe /Delete /F /TN $serviceName *> $null

Get-Process -ErrorAction SilentlyContinue |
  Where-Object {
    $_.ProcessName -eq 'hotswitch-receiver' -or
    ($_.Path -and $_.Path.EndsWith('hotswitch-receiver.exe'))
  } |
  Stop-Process -Force -ErrorAction SilentlyContinue

if (Test-Path $InstallDir) {
  Remove-Item -Recurse -Force $InstallDir
}

Write-Host 'Hotswitch uninstalled'
