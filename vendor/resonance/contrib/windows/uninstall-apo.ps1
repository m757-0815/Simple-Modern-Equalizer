# Remove the Resonance APO: restore each render endpoint's original effect
# slots, unregister the COM class + APO catalog, and clear the signature-check
# override. Run elevated (Inno Setup [UninstallRun]).
$ErrorActionPreference = 'Continue'

# Stop only the daemon and desktop app installed alongside this script.
$programNames = @('resonanced', 'simple-modern-equalizer', 'simpledarkeq')
$ownedPrograms = $programNames | ForEach-Object { Join-Path $PSScriptRoot "$_.exe" }
Get-Process -Name $programNames -EA SilentlyContinue | Where-Object {
  $_.Path -in $ownedPrograms
} | Stop-Process -Force -EA SilentlyContinue

$clsid = '{7C3D2A1E-9B6F-4E2A-8D5C-1F0A3B4C5D6E}'
$backupRoot = 'HKLM:\SOFTWARE\Resonance\ApoBackup'

Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Priv {
  [StructLayout(LayoutKind.Sequential)] public struct LUID { public uint Lo; public int Hi; }
  [StructLayout(LayoutKind.Sequential)] public struct LAA { public LUID Luid; public uint Attr; }
  [StructLayout(LayoutKind.Sequential)] public struct TP { public uint Count; public LAA Priv; }
  [DllImport("advapi32.dll", SetLastError=true)] public static extern bool OpenProcessToken(IntPtr h, uint a, out IntPtr t);
  [DllImport("advapi32.dll", SetLastError=true)] public static extern bool LookupPrivilegeValue(string s, string n, out LUID l);
  [DllImport("advapi32.dll", SetLastError=true)] public static extern bool AdjustTokenPrivileges(IntPtr t, bool d, ref TP n, uint len, IntPtr p, IntPtr r);
  [DllImport("kernel32.dll")] public static extern IntPtr GetCurrentProcess();
  public static void Enable(string name){ IntPtr tok; OpenProcessToken(GetCurrentProcess(),0x28,out tok); LUID l; LookupPrivilegeValue(null,name,out l); TP tp; tp.Count=1; tp.Priv.Luid=l; tp.Priv.Attr=2; AdjustTokenPrivileges(tok,false,ref tp,0,IntPtr.Zero,IntPtr.Zero); }
}
"@
[Priv]::Enable('SeTakeOwnershipPrivilege'); [Priv]::Enable('SeRestorePrivilege'); [Priv]::Enable('SeBackupPrivilege')

$renderSub = 'SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render'
$render = "HKLM:\$renderSub"
$fx   = '{D04E05A6-594B-4FB6-A80D-01AF5EED7D1D}'
$mode = '{D3993A3F-99C2-4402-B5EC-A92A0367664B}'

# Read the saved prior signature-check state before the backup tree is removed,
# so we can decide whether to clear DisableProtectedAudioDG below.
$priorDpadg = (Get-ItemProperty -Path $backupRoot -Name 'DisableProtectedAudioDG_prior' -EA SilentlyContinue).'DisableProtectedAudioDG_prior'

# restore each endpoint we touched from the backup
if (Test-Path $backupRoot) {
  $failed = @()
  Get-ChildItem $backupRoot -EA SilentlyContinue | ForEach-Object {
    $backup = $_
    $g = $backup.PSChildName
    $fxp = "$render\$g\FxProperties"
    if (-not (Test-Path $fxp)) { return }
    try {
      $live = Get-Item -LiteralPath $fxp -EA Stop
      foreach ($index in '14','15') {
        $name = "$fx,$index"
        $effects = @($live.GetValue($name))
        if ($effects -contains $clsid) {
          $remaining = [string[]]@($effects | Where-Object { $_ -ne $clsid })
          New-ItemProperty -Force -Path $fxp -Name $name -PropertyType MultiString -Value $remaining -EA Stop | Out-Null
        }
      }
      $names = $backup.GetValueNames()
      foreach ($slot in '1','2','5','6','7') {
        foreach ($name in @("$fx,$slot", "$mode,$slot")) {
          if ($names -notcontains $name) { continue }
          $prior = $backup.GetValue($name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
          if ($prior -is [string] -and $prior -eq '<none>') {
            if ((Get-Item -LiteralPath $fxp -EA Stop).GetValueNames() -contains $name) {
              Remove-ItemProperty -Path $fxp -Name $name -EA Stop
            }
          } else {
            New-ItemProperty -Force -Path $fxp -Name $name -PropertyType ($backup.GetValueKind($name).ToString()) -Value $prior -EA Stop | Out-Null
          }
        }
      }
    } catch {
      $failed += $g
      Write-Error "Could not restore audio endpoint ${g}: $($_.Exception.Message)"
    }
  }
  if ($failed.Count -gt 0) { exit 1 }
}

# unregister COM class + APO catalog
Remove-Item -Recurse -Force "HKLM:\SOFTWARE\Classes\CLSID\$clsid" -EA SilentlyContinue
Remove-Item -Recurse -Force "HKLM:\SOFTWARE\Classes\AudioEngine\AudioProcessingObjects\$clsid" -EA SilentlyContinue

# Restore the signature-check override to its pre-install state. Only clear it
# if the backup records that it didn't exist before we installed ('<none>') — otherwise
# another unsigned APO (e.g. EqualizerAPO) set it and still needs it.
$audioKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Audio'
if ($priorDpadg -eq '<none>') {
  Remove-ItemProperty -Path $audioKey -Name 'DisableProtectedAudioDG' -EA SilentlyContinue
} elseif ($null -ne $priorDpadg) {
  Set-ItemProperty -Path $audioKey -Name 'DisableProtectedAudioDG' -Value ([int]$priorDpadg) -Type DWord
}
Remove-Item -Recurse -Force $backupRoot -EA SilentlyContinue

# drop the daemon's shared state file
Remove-Item -Force 'C:\ProgramData\Resonance\apo_state.bin' -EA SilentlyContinue

Restart-Service audiosrv -Force -EA SilentlyContinue
Write-Output 'Resonance APO removed.'
