param(
    [string] $WixPath = 'wix',
    [string] $WixExtension = 'WixToolset.Util.wixext/4.0.6'
)
$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Push-Location $projectRoot
try {
    $metadata = cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE) { throw 'Could not read the app version' }
    $version = ($metadata | ConvertFrom-Json).packages[0].version
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    cargo build --locked --release
    if ($LASTEXITCODE) { throw 'Simple Modern Equalizer build failed' }
    cargo build --locked --manifest-path vendor/resonance/Cargo.toml --release -p resonance-daemon
    if ($LASTEXITCODE) { throw 'Audio engine build failed' }
    Push-Location $projectRoot
    try {
        & ./vendor/resonance/contrib/windows/build-apo.ps1
        if ($LASTEXITCODE) { throw 'Windows audio component build failed' }
    } finally { Pop-Location }
    $stage = Join-Path $projectRoot 'dist/windows'
    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    Copy-Item -LiteralPath 'target/release/simple-modern-equalizer.exe' -Destination $stage
    Copy-Item -LiteralPath 'vendor/resonance/target/release/resonanced.exe' -Destination $stage
    Copy-Item -LiteralPath 'vendor/resonance/target/apo/resonance_apo.dll' -Destination $stage
    Copy-Item -LiteralPath 'vendor/resonance/contrib/windows/install-apo.ps1' -Destination $stage
    Copy-Item -LiteralPath 'vendor/resonance/contrib/windows/uninstall-apo.ps1' -Destination $stage
    Copy-Item -LiteralPath 'LICENSE','THIRD_PARTY_NOTICES.md' -Destination $stage
    Copy-Item -LiteralPath 'ui/fonts/OFL.txt' -Destination (Join-Path $stage 'Inter-OFL.txt')
    $installer = Join-Path $projectRoot 'dist/SimpleModernEqualizer-Setup-x64.msi'
    $wixArgs = @('build', 'packaging/windows.wxs', '-arch', 'x64', '-ext', $WixExtension,
        '-d', "StageDir=$stage", '-d', "ProductVersion=$version", '-o', $installer)
    if ($WixPath.EndsWith('.dll', [StringComparison]::OrdinalIgnoreCase)) {
        dotnet $WixPath @wixArgs
    } else {
        & $WixPath @wixArgs
    }
    if ($LASTEXITCODE) { throw 'WiX installer build failed' }
    Write-Output $installer
} finally { Pop-Location }
