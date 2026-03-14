param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$serviceName = 'Hotswitch'

function Assert-Admin {
  $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
  $principal = New-Object Security.Principal.WindowsPrincipal($identity)
  if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run start-hotswitch.ps1 from an elevated PowerShell prompt.'
  }
}

Assert-Admin
sc.exe start $serviceName
