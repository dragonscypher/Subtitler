#requires -Version 5.1
[CmdletBinding(SupportsShouldProcess, ConfirmImpact = 'Medium')]
param(
    [Parameter(Mandatory)]
    [string]$ExtensionId,

    [Parameter(Mandatory)]
    [string]$HostExecutable,

    [string]$InstallDirectory = (Join-Path $env:LOCALAPPDATA 'Subtitler\developer\native-messaging')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Import-Module (Join-Path $PSScriptRoot 'native-host-registration.psm1') -Force

$validatedExtensionId = Assert-SubtitlerExtensionId -ExtensionId $ExtensionId
$resolvedExecutable = Resolve-SubtitlerHostExecutable -HostExecutable $HostExecutable -MustExist
$resolvedInstallDirectory = Resolve-SubtitlerDeveloperInstallDirectory -InstallDirectory $InstallDirectory
$manifestPath = Get-SubtitlerNativeHostManifestPath -InstallDirectory $resolvedInstallDirectory
$hostName = Get-SubtitlerNativeHostName
$registryPath = Get-SubtitlerNativeHostRegistryPath

# Never overwrite a registration that could belong to a packaged or unrelated
# installation. A prior developer registration is reusable only when it points
# to this exact developer-owned manifest path.
$existingRegistration = Get-SubtitlerNativeHostRegistration
if ($null -ne $existingRegistration) {
    $existingManifestPath = Resolve-SubtitlerLocalPath -Path $existingRegistration.ManifestPath
    if (-not (Test-SubtitlerPathEqual -Left $existingManifestPath -Right $manifestPath)) {
        throw "A different $hostName registration already exists. Refusing to replace it; inspect or unregister that exact developer registration first."
    }
}

if ($WhatIfPreference) {
    if (-not (Test-Path -LiteralPath $resolvedInstallDirectory -PathType Container)) {
        $null = $PSCmdlet.ShouldProcess($resolvedInstallDirectory, 'Create the developer-owned manifest directory')
    }
    $null = $PSCmdlet.ShouldProcess($manifestPath, 'Atomically write a private native-host manifest')
    $null = $PSCmdlet.ShouldProcess($registryPath, 'Register the per-user Chrome Native Messaging host')
    Write-Host "WhatIf validation passed for $hostName and chrome-extension://$validatedExtensionId/."
    return
}

if (-not (Test-Path -LiteralPath $resolvedInstallDirectory -PathType Container)) {
    if (-not $PSCmdlet.ShouldProcess($resolvedInstallDirectory, 'Create the developer-owned manifest directory')) {
        return
    }

    New-Item -ItemType Directory -Path $resolvedInstallDirectory -Force | Out-Null
}

# Re-check after directory creation so a race or reparse-point substitution
# cannot turn the following atomic write into a write outside the owned root.
$null = Resolve-SubtitlerLocalPath -Path $resolvedInstallDirectory -ExpectedType Container -MustExist

if (-not $PSCmdlet.ShouldProcess($manifestPath, 'Atomically write a private native-host manifest')) {
    return
}

$writeManifestParameters = @{
    ManifestPath    = $manifestPath
    ExtensionId     = $validatedExtensionId
    HostExecutable  = $resolvedExecutable
}
$writtenManifestPath = Write-SubtitlerNativeHostManifestAtomically @writeManifestParameters

if (-not $PSCmdlet.ShouldProcess($registryPath, 'Register the per-user Chrome Native Messaging host')) {
    Write-Warning "The manifest was written but $hostName was not registered. Re-run this script when ready."
    return
}

Set-SubtitlerNativeHostRegistration -ManifestPath $writtenManifestPath

Write-Host "Registered $hostName for chrome-extension://$validatedExtensionId/."
Write-Host "Manifest: $writtenManifestPath"
Write-Host "Host:     $resolvedExecutable"
Write-Host 'The developer manifest contains no cookies, tokens, API keys, media URLs, or transcript data.'
