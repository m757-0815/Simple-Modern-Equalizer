# Install the Resonance Audio Processing Object (APO) into the Windows audio
# engine. No driver, no virtual cable: a user-mode COM DLL that audiodg.exe
# loads as a system-effect APO on the active render endpoints.
#
# Run elevated (the Inno Setup [Run] section does this). Parameters:
#   -DllPath   full path to resonance_apo.dll (installed under {app})
[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)] [string] $DllPath
)
$ErrorActionPreference = 'Continue'

# Transcript so the (run-hidden) installer's APO registration + per-endpoint
# attach results are captured - the only way to diagnose attach failures on
# machines we can't reach (real audio drivers, Win editions, AV, etc.).
$logDir = Join-Path $env:ProgramData 'Resonance'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
try { Start-Transcript -Path (Join-Path $logDir 'apo-install.log') -Force | Out-Null } catch {}
Write-Output "install-apo: DllPath=$DllPath exists=$(Test-Path $DllPath) OS=$([Environment]::OSVersion.Version)"

# The daemon (logged-in user) creates %ProgramData%\Resonance\apo_state.bin; the
# APO inside audiodg.exe must also open it READ-WRITE (seqlock + telemetry). When
# audiodg runs as the restricted LocalService token it would get ACCESS_DENIED on
# the user-created file, the APO falls back to flat passthrough, and there's no
# EQ/spectrum. Grant the dir (with inheritance) to LocalService + NetworkService
# + Users so the daemon-created file inherits write access for audiodg.
& icacls.exe "$logDir" /grant '*S-1-5-19:(OI)(CI)F' '*S-1-5-20:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)M' 2>&1 | Out-Null
Write-Output "granted ACLs on $logDir (icacls exit $LASTEXITCODE)"

$clsid = '{7C3D2A1E-9B6F-4E2A-8D5C-1F0A3B4C5D6E}'
$backupRoot = 'HKLM:\SOFTWARE\Resonance\ApoBackup'

# --- privileges needed to take ownership of SYSTEM-owned MMDevices keys ---
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
$admins = New-Object System.Security.Principal.SecurityIdentifier('S-1-5-32-544')

function Grant-Key([string]$sub) {
  if (-not (Test-Path "HKLM:\$sub")) { return }
  $k = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($sub,
        [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree,
        [System.Security.AccessControl.RegistryRights]::TakeOwnership)
  $a = $k.GetAccessControl([System.Security.AccessControl.AccessControlSections]::None)
  $a.SetOwner($admins); $k.SetAccessControl($a); $k.Close()
  $k = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($sub,
        [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree,
        [System.Security.AccessControl.RegistryRights]::ChangePermissions)
  $a = $k.GetAccessControl()
  $rule = New-Object System.Security.AccessControl.RegistryAccessRule($admins,'FullControl','ContainerInherit','None','Allow')
  $a.AddAccessRule($rule); $k.SetAccessControl($a); $k.Close()
}

# --- 1) COM in-proc server ---
$base = "HKLM:\SOFTWARE\Classes\CLSID\$clsid"
New-Item -Force -Path "$base\InprocServer32" | Out-Null
Set-ItemProperty -Path $base -Name '(default)' -Value 'Resonance Audio Processing Object'
Set-ItemProperty -Path "$base\InprocServer32" -Name '(default)' -Value $DllPath
Set-ItemProperty -Path "$base\InprocServer32" -Name 'ThreadingModel' -Value 'Both'

# --- 2) APO catalog (audiodg ignores FxProperties CLSIDs not registered here) ---
$cat = "HKLM:\SOFTWARE\Classes\AudioEngine\AudioProcessingObjects\$clsid"
New-Item -Force -Path $cat | Out-Null
Set-ItemProperty -Path $cat -Name 'FriendlyName' -Value 'Resonance APO'
Set-ItemProperty -Path $cat -Name 'Copyright'    -Value 'Resonance'
# MaxInstances is "unlimited" = 0xFFFFFFFF. We store it as -1: Windows PowerShell
# 5.1 converts a DWord value through Int32, so the literal 4294967295 overflows
# and the property is silently never written. -1 has the identical 32-bit pattern.
foreach ($kv in @{MajorVersion=0;MinorVersion=5;MinInputConnections=1;MaxInputConnections=1;MinOutputConnections=1;MaxOutputConnections=1;MaxInstances=-1;Flags=15;NumAPOInterfaces=1}.GetEnumerator()) {
  New-ItemProperty -Force -Path $cat -Name $kv.Key -PropertyType DWord -Value $kv.Value | Out-Null
}
Set-ItemProperty -Path $cat -Name 'APOInterface0' -Value '{FD7F2B29-24D0-4B5C-B177-592C39F9CA10}'

# --- 3) disable APO signature check so the unsigned APO loads in audiodg ---
$audioKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Audio'
if (-not (Test-Path $audioKey)) { New-Item -Path $audioKey -Force | Out-Null }
# Record the prior value (or '<none>') so uninstall only clears the override if
# WE created it — another unsigned APO (e.g. EqualizerAPO) may also rely on it.
$priorDpadg = (Get-ItemProperty -Path $audioKey -Name 'DisableProtectedAudioDG' -EA SilentlyContinue).DisableProtectedAudioDG
if (-not (Test-Path $backupRoot)) { New-Item -Force -Path $backupRoot -EA Stop | Out-Null }
if ($null -eq (Get-ItemProperty -Path $backupRoot -Name 'DisableProtectedAudioDG_prior' -EA SilentlyContinue).DisableProtectedAudioDG_prior) {
  $saved = if ($null -eq $priorDpadg) { '<none>' } else { [string]$priorDpadg }
  Set-ItemProperty -Path $backupRoot -Name 'DisableProtectedAudioDG_prior' -Value $saved -EA Stop
}
Set-ItemProperty -Path $audioKey -Name 'DisableProtectedAudioDG' -Value 1 -Type DWord -EA Stop

# --- 4) attach to each render endpoint, ONE slot per endpoint ---
# Register our chain once per endpoint, preserving other audio effects. When the
# device exposes a composite list, append to that list in processing order.
$renderSub = 'SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render'
$render = "HKLM:\$renderSub"
$fx   = '{D04E05A6-594B-4FB6-A80D-01AF5EED7D1D}'   # PKEY_FX_*  ,1/,2 legacy  ,5/,6/,7 SFX/MFX/EFX
$mode = '{D3993A3F-99C2-4402-B5EC-A92A0367664B}'   # per-slot processing-mode list
$defaultMode = '{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}'   # DEFAULT processing mode
$combinedName = '{b3f8fa53-0004-438e-9003-51a46e139bfc},41'   # combined-device flag
# Backups describe the machine as it was BEFORE our first install. On an upgrade/reinstall the
# live values are already ours, so re-recording them would make uninstall "restore" our own APO
# and leave DisableProtectedAudioDG on (and lose the driver's original SFX/MFX entries): keep
# any backup that already exists and only record what is still missing.
$allSlots = '1','2','5','6','7'

$attached = 0
$verified = 0
$activeCount = 0
$activeVerified = 0
Get-ChildItem $render -EA SilentlyContinue | ForEach-Object {
  $g = $_.PSChildName
  $active = ($_.GetValue("DeviceState") -band 1) -eq 1
  if ($active) { $activeCount++ }
  try {
    Grant-Key "$renderSub\$g"
    $fxp = "$render\$g\FxProperties"
    if (Test-Path $fxp) { Grant-Key "$renderSub\$g\FxProperties" } else { New-Item -Path $fxp -EA Stop | Out-Null }

    # Back up effect and processing-mode values, including their registry types.
    $bk = "$backupRoot\$g"
    if (-not (Test-Path $bk)) {   # an existing backup is the pre-install original: never overwrite it
      New-Item -Force -Path $bk -EA Stop | Out-Null
      $sourceKey = Get-Item -LiteralPath $fxp -EA Stop
      $properties = $sourceKey.GetValueNames()
      foreach ($slot in $allSlots) {
        foreach ($name in @("$fx,$slot", "$mode,$slot")) {
          if ($properties -contains $name) {
            $prior = $sourceKey.GetValue($name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            New-ItemProperty -Path $bk -Name $name -PropertyType ($sourceKey.GetValueKind($name).ToString()) -Value $prior -EA Stop | Out-Null
          } else {
            New-ItemProperty -Path $bk -Name $name -PropertyType String -Value '<none>' -EA Stop | Out-Null
          }
        }
      }
    }

    $key = Get-Item -LiteralPath $fxp -EA Stop
    $combined = $null -ne (Get-ItemProperty -Path "$render\$g\Properties" -Name $combinedName -EA SilentlyContinue).$combinedName
    $slot = if ($combined) { '6' } else { '7' }
    $composite = if ($combined) { '14' } else { '15' }
    $isComposite = $key.GetValueNames() -contains "$fx,$composite"
    if ($isComposite) {
      # Keep the device manufacturer's effects in their original order.
      $name = "$fx,$composite"
      $effects = @($key.GetValue($name) | Where-Object { $_ -ne $clsid })
      New-ItemProperty -Force -Path $fxp -Name $name -PropertyType MultiString -Value ([string[]]($effects + $clsid)) -EA Stop | Out-Null
    } else {
      $legacy = $key.GetValue("$fx,1") -or $key.GetValue("$fx,2")
      $modern = $key.GetValue("$fx,5") -or $key.GetValue("$fx,6") -or $key.GetValue("$fx,7")
      if ($legacy -and -not $modern) { $slot = '2' }
      $name = "$fx,$slot"
      Set-ItemProperty -Path $fxp -Name $name -Value $clsid -EA Stop
    }
    foreach ($old in $allSlots) {
      if ("$fx,$old" -ne $name -and $key.GetValue("$fx,$old") -eq $clsid) {
        Remove-ItemProperty -Path $fxp -Name "$fx,$old" -EA Stop
      }
    }
    if ($slot -ne '2') {
      $modes = @($key.GetValue("$mode,$slot") | Where-Object { $_ })
      if ($modes -notcontains $defaultMode) {
        New-ItemProperty -Force -Path $fxp -Name "$mode,$slot" -PropertyType MultiString -Value ([string[]]($modes + $defaultMode)) -EA Stop | Out-Null
      }
    }
    $attached++
    $rb = (Get-Item -LiteralPath $fxp -EA Stop).GetValue($name)
    if (@($rb) -contains $clsid) { if ($active) { $activeVerified++ }; $verified++; Write-Output "attached+verified $g property=$name" }
    else { Write-Output "attach read-back mismatch $g property=$name" }
  } catch {
    Write-Output "attach FAILED $g : $($_.Exception.Message)"
  }
}
Write-Output "attach summary: attached=$attached verified=$verified"
if ($verified -eq 0) {
  Write-Output 'ERROR: APO did not verify on ANY render endpoint - EQ will not work. The default playback device may be vendor-locked or protected.'
}

# --- 5) restart the audio engine so audiodg reloads APO registrations ---
Restart-Service audiosrv -Force -EA SilentlyContinue
Write-Output 'Resonance APO install finished.'
if ($activeCount -gt 0 -and $activeVerified -eq 0) {
  Write-Output "ERROR: registration failed on every active playback device."
}
try { Stop-Transcript | Out-Null } catch {}
if ($verified -eq 0 -or ($activeCount -gt 0 -and $activeVerified -eq 0)) { exit 1 }
