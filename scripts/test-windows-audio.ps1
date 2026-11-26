# Exercise the installed Windows APO on the default playback device.
[CmdletBinding()]
param([switch]$SelfTest)
$ErrorActionPreference = 'Stop'

#######################################################
# read enabled callbacks from the Windows audio engine
#######################################################

function Get-Callback([string[]]$Lines, [int[]]$AudioEngineIds) {
    foreach ($line in $Lines) {
        if ($line -match '^pid=(\d+) process #\d+: in_peak=[\d.]+ in_rms=([\d.]+) frames=[1-9]\d* ch=[1-9]\d* enabled=(true|false) ') {
            $engine = [int]$Matches[1]
            if ($engine -in $AudioEngineIds -and $Matches[3] -eq 'true') {
                [pscustomobject]@{ Engine = $engine; Rms = [double]::Parse($Matches[2], [cultureinfo]::InvariantCulture) }
            }
        }
    }
}

function Test-TonePattern($Phases) {
    foreach ($engine in @($Phases[1] | Select-Object -ExpandProperty Engine -Unique)) {
        $levels = foreach ($phase in $Phases) {
            $values = @($phase | Where-Object { $_.Engine -eq $engine } | Select-Object -ExpandProperty Rms | Sort-Object)
            if ($values.Count -lt 2) { break }
            $values[[int][math]::Floor(($values.Count - 1) / 2)]
        }
        if ($levels.Count -eq 3 -and $levels[0] -gt 0 -and $levels[2] -gt 0 -and
            $levels[1] -ge 3 * [math]::Max($levels[0], $levels[2]) -and
            [math]::Max($levels[0], $levels[2]) -le 2 * [math]::Min($levels[0], $levels[2])) { return $true }
    }
    return $false
}

#######################################################
# generate a quiet test tone in memory
#######################################################

function New-Tone([double]$Amplitude) {
    $rate = 24000
    $count = 4 * $rate
    $stream = [IO.MemoryStream]::new()
    $writer = [IO.BinaryWriter]::new($stream, [Text.Encoding]::ASCII, $true)
    try {
        $writer.Write([Text.Encoding]::ASCII.GetBytes('RIFF'))
        $writer.Write([int](36 + 2 * $count))
        $writer.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt '))
        $writer.Write([int]16)
        $writer.Write([int16]1)
        $writer.Write([int16]1)
        $writer.Write([int]$rate)
        $writer.Write([int](2 * $rate))
        $writer.Write([int16]2)
        $writer.Write([int16]16)
        $writer.Write([Text.Encoding]::ASCII.GetBytes('data'))
        $writer.Write([int](2 * $count))
        for ($i = 0; $i -lt $count; $i++) {
            $fade = [math]::Min(1, [math]::Min($i, $count - 1 - $i) / 240)
            $writer.Write([int16]($Amplitude * 32767 * $fade * [math]::Sin(2 * [math]::PI * 600 * $i / $rate)))
        }
    } finally { $writer.Dispose() }
    $stream.Position = 0
    return $stream
}

#######################################################
# check the detector against recorded fixtures
#######################################################

if ($SelfTest) {
    foreach ($case in 'working','daemon-only','disabled','constant','other-process','missing-phase') {
        $phases = [collections.ArrayList]::new()
        foreach ($level in '0.0030','0.0240','0.0030') {
            $enabled = if ($case -eq 'disabled') { 'false' } else { 'true' }
            $rms = if ($case -eq 'constant') { '0.0200' } else { $level }
            $engine = if ($case -eq 'other-process') { 88 } else { 42 }
            $line = "pid=$engine process #100: in_peak=0.04 in_rms=$rms frames=480 ch=2 enabled=$enabled preamp=0 filters=1"
            if ($case -eq 'daemon-only' -or ($case -eq 'missing-phase' -and $phases.Count -eq 2)) { $line = '' }
            [void]$phases.Add(@(Get-Callback @($line,$line) @(42)))
        }
        if ((Test-TonePattern $phases) -ne ($case -eq 'working')) { throw "Unexpected verdict for $case" }
    }
    Write-Output 'PASS - all six diagnostic fixtures.'
    exit 0
}

#######################################################
# play the test tones and verify that the audio effect receives them
#######################################################

$log = if ($env:RESONANCE_APO_LOG) { $env:RESONANCE_APO_LOG } else { Join-Path $env:ProgramData 'Resonance\apo.log' }
Write-Output 'Playing three quiet tones on the default playback device. Pause other audio and leave EQ enabled.'
$phases = [collections.ArrayList]::new()
foreach ($amplitude in 0.006,0.048,0.006) {
    $stream = New-Tone $amplitude
    $player = [System.Media.SoundPlayer]::new($stream)
    try {
        # Use the log position before playback to isolate callbacks for this tone.
        $offset = if (Test-Path -LiteralPath $log) { (Get-Item -LiteralPath $log).Length } else { 0 }
        $player.PlaySync()
        $lines = @()
        if (Test-Path -LiteralPath $log) {
            $file = [IO.File]::Open($log, 'Open', 'Read', [IO.FileShare]::ReadWrite)
            try {
                if ($file.Length -lt $offset) { throw 'APO log rotated during playback. Repeat the test.' }
                $file.Position = $offset
                $reader = [IO.StreamReader]::new($file)
                try { $lines = $reader.ReadToEnd() -split '\r?\n' } finally { $reader.Dispose() }
            } finally { $file.Dispose() }
        }
        $ids = @(Get-Process audiodg -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id)
        [void]$phases.Add(@(Get-Callback $lines $ids))
    } finally { $player.Dispose(); $stream.Dispose() }
}
if (Test-TonePattern $phases) {
    Write-Output 'PASS - the enabled APO received the quiet/louder/quiet signal in the Windows audio engine.'
    exit 0
}
$counts = @($phases | ForEach-Object { $_.Count }) -join ', '
Write-Output "FAIL - could not verify the tone in the enabled APO. Fresh enabled callbacks per phase: $counts."
Write-Output "Check that EQ and Windows audio enhancements are enabled on the default output. Callback log: $log"
exit 1
