[CmdletBinding()]
param(
    [switch]$SkipExtension,
    [switch]$SkipNative
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$projectRoot = Split-Path -Parent $PSScriptRoot
$extensionRoot = Join-Path $projectRoot 'extension'
$nativeManifest = Join-Path $projectRoot 'native\Cargo.toml'

function Invoke-CheckedCommand {
    param(
        [Parameter(Mandatory)] [string]$FilePath,
        [Parameter(Mandatory)] [string[]]$Arguments,
        [Parameter(Mandatory)] [string]$Description
    )

    Write-Host "`n==> $Description"
    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Description failed with exit code $LASTEXITCODE."
    }
}

if (-not $SkipExtension) {
    if (-not (Test-Path -LiteralPath (Join-Path $extensionRoot 'node_modules'))) {
        throw 'Extension dependencies are absent. Run: npm --prefix extension install'
    }

    Invoke-CheckedCommand -FilePath 'npm' -Arguments @('--prefix', $extensionRoot, 'run', 'typecheck') -Description 'Extension type check'
    Invoke-CheckedCommand -FilePath 'npm' -Arguments @('--prefix', $extensionRoot, 'test') -Description 'Extension unit tests'
    Invoke-CheckedCommand -FilePath 'npm' -Arguments @('--prefix', $extensionRoot, 'run', 'build') -Description 'Extension production build'
}

if (-not $SkipNative) {
    $cargoCommand = Get-Command cargo -ErrorAction SilentlyContinue
    $cargoPath = $null
    if ($null -ne $cargoCommand) {
        $cargoPath = $cargoCommand.Source
    }
    if ([string]::IsNullOrWhiteSpace($cargoPath)) {
        $userCargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
        if (Test-Path -LiteralPath $userCargo) {
            $cargoPath = $userCargo
        }
    }
    if ([string]::IsNullOrWhiteSpace($cargoPath)) {
        throw 'Rust Cargo was not found. Install Rust stable and retry.'
    }

    Invoke-CheckedCommand -FilePath $cargoPath -Arguments @('fmt', '--manifest-path', $nativeManifest, '--all', '--', '--check') -Description 'Rust formatting check'
    Invoke-CheckedCommand -FilePath $cargoPath -Arguments @('test', '--manifest-path', $nativeManifest, '--workspace', '--locked') -Description 'Rust workspace tests'
    Invoke-CheckedCommand -FilePath $cargoPath -Arguments @('clippy', '--manifest-path', $nativeManifest, '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings') -Description 'Rust lint'
    Invoke-CheckedCommand -FilePath $cargoPath -Arguments @('build', '--manifest-path', $nativeManifest, '--release', '-p', 'subtitler-native-host', '--locked') -Description 'Native host release build'
    Invoke-CheckedCommand -FilePath 'powershell' -Arguments @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $projectRoot 'scripts\test-native-host-registration.ps1')) -Description 'Native-host registration validation (no registry writes)'
    Invoke-CheckedCommand -FilePath 'powershell' -Arguments @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $projectRoot 'scripts\test-real-local-pipeline-contract.ps1')) -Description 'Real local-pipeline helper contract validation (no media run)'
    Invoke-CheckedCommand -FilePath 'powershell' -Arguments @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $projectRoot 'scripts\test-native-host.ps1')) -Description 'Native Messaging smoke test'
}

Write-Host "`nAll requested verification steps passed."
