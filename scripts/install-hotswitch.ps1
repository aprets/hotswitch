param(
  [string]$InstallDir = "$env:ProgramFiles\Hotswitch",
  [string]$SourceDir = '',
  [string]$ReleaseTag = '',
  [string]$RepoOwner = 'aprets',
  [string]$RepoName = 'hotswitch',
  [int]$WaitPid = 0
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$serviceName = 'Hotswitch'
$firewallRuleName = 'Hotswitch Receiver UDP 24801'
$archiveName = 'hotswitch-receiver-x86_64-pc-windows-msvc.zip'
$requiredFiles = @(
  'hotswitch-receiver.exe',
  'hotswitch-receiver-service.exe',
  'install-hotswitch.ps1',
  'start-hotswitch.ps1',
  'uninstall-hotswitch.ps1'
)

function Assert-Admin {
  $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
  $principal = New-Object Security.Principal.WindowsPrincipal($identity)
  if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run install-hotswitch.ps1 from an elevated PowerShell prompt.'
  }
}

function Stop-HotswitchService {
  $service = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
  if (-not $service) {
    return
  }

  if ($service.Status -ne 'Stopped') {
    sc.exe stop $serviceName | Out-Null
    $deadline = (Get-Date).AddSeconds(20)
    do {
      Start-Sleep -Milliseconds 500
      $service.Refresh()
      if ($service.Status -eq 'Stopped') {
        break
      }
    } while ((Get-Date) -lt $deadline)
  }
}

function Stop-HotswitchReceiverProcesses {
  Get-Process -ErrorAction SilentlyContinue |
    Where-Object {
      $_.ProcessName -eq 'hotswitch-receiver' -or
      ($_.Path -and $_.Path.EndsWith('hotswitch-receiver.exe'))
    } |
    Stop-Process -Force -ErrorAction SilentlyContinue
}

function Remove-LegacyStartupTask {
  schtasks.exe /Delete /F /TN $serviceName *> $null
}

function Resolve-PayloadDir {
  if ($ReleaseTag) {
    $tempRoot = Join-Path $env:TEMP ("hotswitch-update-" + [guid]::NewGuid().ToString('N'))
    $zipPath = Join-Path $tempRoot $archiveName
    $extractDir = Join-Path $tempRoot 'payload'
    New-Item -ItemType Directory -Force -Path $tempRoot | Out-Null

    $zipUrl = "https://github.com/$RepoOwner/$RepoName/releases/download/$ReleaseTag/$archiveName"
    Invoke-WebRequest -Uri $zipUrl -OutFile $zipPath
    Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force
    return $extractDir
  }

  if ($SourceDir) {
    return (Resolve-Path $SourceDir).Path
  }

  return $PSScriptRoot
}

function Assert-PayloadFiles([string]$PayloadDir) {
  foreach ($file in $requiredFiles) {
    $path = Join-Path $PayloadDir $file
    if (-not (Test-Path $path -PathType Leaf)) {
      throw "Missing required payload file: $path"
    }
  }
}

function Install-ServiceBinary([string]$ServiceExe) {
  $service = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
  if ($service) {
    sc.exe config $serviceName binPath= "`"$ServiceExe`"" start= auto | Out-Null
  } else {
    sc.exe create $serviceName binPath= "`"$ServiceExe`"" start= auto | Out-Null
    sc.exe description $serviceName "Hotswitch session launcher service" | Out-Null
  }
}

function Install-FirewallRule([string]$ReceiverExe) {
  Get-NetFirewallRule -DisplayName $firewallRuleName -ErrorAction SilentlyContinue |
    Remove-NetFirewallRule -ErrorAction SilentlyContinue

  New-NetFirewallRule `
    -DisplayName $firewallRuleName `
    -Direction Inbound `
    -Profile Private `
    -Action Allow `
    -Protocol UDP `
    -LocalPort 24801 `
    -Program $ReceiverExe | Out-Null
}

Assert-Admin

if ($WaitPid -gt 0) {
  Wait-Process -Id $WaitPid -Timeout 30 -ErrorAction SilentlyContinue
}

$payloadDir = Resolve-PayloadDir
Assert-PayloadFiles -PayloadDir $payloadDir

Stop-HotswitchService
Stop-HotswitchReceiverProcesses
Remove-LegacyStartupTask

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
foreach ($file in $requiredFiles) {
  Copy-Item -Force (Join-Path $payloadDir $file) (Join-Path $InstallDir $file)
}

$receiverExe = Join-Path $InstallDir 'hotswitch-receiver.exe'
$serviceExe = Join-Path $InstallDir 'hotswitch-receiver-service.exe'
Install-FirewallRule -ReceiverExe $receiverExe
Install-ServiceBinary -ServiceExe $serviceExe
sc.exe start $serviceName | Out-Null

Write-Host "Hotswitch installed to $InstallDir"
