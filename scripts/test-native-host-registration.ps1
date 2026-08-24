#requires -Version 5.1
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Import-Module (Join-Path $PSScriptRoot 'native-host-registration.psm1') -Force

function Assert-Throws {
    param(
        [Parameter(Mandatory)]
        [scriptblock]$Action,

        [Parameter(Mandatory)]
        [string]$Description
    )

    try {
        & $Action | Out-Null
    }
    catch {
        return
    }

    throw "Expected validation failure: $Description"
}

function Assert-ScriptParses {
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    $tokens = $null
    $parseErrors = $null
    [void][System.Management.Automation.Language.Parser]::ParseFile(
        $Path,
        [ref]$tokens,
        [ref]$parseErrors
    )
    if ($parseErrors.Count -ne 0) {
        throw "PowerShell parser rejected $(Split-Path -Leaf $Path): $($parseErrors[0].Message)"
    }
}

$extensionId = 'abcdefghijklmnopabcdefghijklmnop'
$developerRoot = Get-SubtitlerDeveloperInstallRoot
$testRoot = Join-Path $developerRoot ("registration-validation-$([guid]::NewGuid().ToString('N'))")
$testRootCreated = $false

try {
    # This test never calls the registration or unregistration commands and
    # never opens HKCU for writing. It uses only a GUID-named fixture below the
    # developer-owned root, then removes that exact fixture in finally.
    New-Item -ItemType Directory -Path $testRoot -Force | Out-Null
    $testRootCreated = $true
    $resolvedTestRoot = Resolve-SubtitlerDeveloperInstallDirectory -InstallDirectory $testRoot
    $hostExecutable = Join-Path $resolvedTestRoot 'subtitler-native-host.exe'
    [System.IO.File]::WriteAllBytes($hostExecutable, [byte[]](0x4D, 0x5A))
    $resolvedHostExecutable = Resolve-SubtitlerHostExecutable -HostExecutable $hostExecutable -MustExist

    $installDirectory = Join-Path $resolvedTestRoot 'native-messaging'
    New-Item -ItemType Directory -Path $installDirectory -Force | Out-Null
    $resolvedInstallDirectory = Resolve-SubtitlerDeveloperInstallDirectory -InstallDirectory $installDirectory
    $manifestPath = Get-SubtitlerNativeHostManifestPath -InstallDirectory $resolvedInstallDirectory

    # Exercise both create and atomic-replace paths. The helper also validates
    # the exact manifest shape and applies the private file ACL before publish.
    $writeParameters = @{
        ManifestPath   = $manifestPath
        ExtensionId    = $extensionId
        HostExecutable = $resolvedHostExecutable
    }
    $firstManifestPath = Write-SubtitlerNativeHostManifestAtomically @writeParameters
    $secondManifestPath = Write-SubtitlerNativeHostManifestAtomically @writeParameters
    if (-not (Test-SubtitlerPathEqual -Left $firstManifestPath -Right $secondManifestPath)) {
        throw 'Atomic manifest replacement returned an unexpected destination path.'
    }

    $validationParameters = @{
        ManifestPath          = $manifestPath
        ExtensionId           = $extensionId
        ExpectedHostExecutable = $resolvedHostExecutable
        RequireHostExecutable = $true
    }
    $validatedManifest = Test-SubtitlerNativeHostManifest @validationParameters
    if (-not (Test-SubtitlerPathEqual -Left $validatedManifest.HostExecutable -Right $resolvedHostExecutable)) {
        throw 'Validated manifest executable path did not match the fixture executable.'
    }

    $manifestAcl = Get-Acl -LiteralPath $manifestPath
    if (-not $manifestAcl.AreAccessRulesProtected) {
        throw 'The developer manifest ACL was not protected from inherited access rules.'
    }

    Assert-Throws -Description 'upper-case extension ID' -Action {
        Assert-SubtitlerExtensionId -ExtensionId 'ABCDEFGHIJKLMNOPABCDEFGHIJKLMNOP'
    }
    Assert-Throws -Description 'relative executable path' -Action {
        Resolve-SubtitlerLocalPath -Path 'relative\subtitler-native-host.exe'
    }
    Assert-Throws -Description 'UNC executable path' -Action {
        Resolve-SubtitlerLocalPath -Path '\\server\share\subtitler-native-host.exe'
    }
    Assert-Throws -Description 'device executable path' -Action {
        Resolve-SubtitlerLocalPath -Path '\\?\C:\Subtitler\subtitler-native-host.exe'
    }
    Assert-Throws -Description 'alternate data stream path' -Action {
        Resolve-SubtitlerLocalPath -Path 'C:\Subtitler\subtitler-native-host.exe:stream'
    }
    Assert-Throws -Description 'install directory outside the owned developer root' -Action {
        Resolve-SubtitlerDeveloperInstallDirectory -InstallDirectory 'C:\Subtitler\native-messaging'
    }

    $malformedManifestPath = Join-Path $resolvedInstallDirectory 'malformed.json'
    [System.IO.File]::WriteAllText(
        $malformedManifestPath,
        '{"name":"com.subtitler.native_host","description":"Subtitler Native Messaging Host (development)","path":"C:\\Windows\\System32\\notepad.exe","type":"stdio","allowed_origins":["chrome-extension://abcdefghijklmnopabcdefghijklmnop/"],"unexpected":true}',
        [System.Text.UTF8Encoding]::new($false)
    )
    Assert-Throws -Description 'manifest with an unexpected property' -Action {
        Test-SubtitlerNativeHostManifest -ManifestPath $malformedManifestPath -ExtensionId $extensionId
    }

    foreach ($scriptName in @(
            'register-native-host.ps1',
            'unregister-native-host.ps1',
            'test-native-host-registration.ps1'
        )) {
        Assert-ScriptParses -Path (Join-Path $PSScriptRoot $scriptName)
    }

    Write-Host 'Native-host registration validation passed (no registry writes performed).'
}
finally {
    if ($testRootCreated -and (Test-Path -LiteralPath $testRoot -PathType Container)) {
        $resolvedCleanupRoot = Resolve-SubtitlerDeveloperInstallDirectory -InstallDirectory $testRoot
        if (-not (Test-SubtitlerPathEqual -Left $resolvedCleanupRoot -Right $testRoot)) {
            throw 'Refusing to remove an unexpected registration-validation directory.'
        }

        Remove-Item -LiteralPath $resolvedCleanupRoot -Recurse -Force
    }
}
