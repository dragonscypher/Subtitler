[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-SubtitlerEnvironmentSnapshot {
    $snapshot = @{}
    Get-ChildItem Env: |
        Where-Object { $_.Name -like 'SUBTITLER_*' } |
        ForEach-Object { $snapshot[$_.Name] = $_.Value }
    return $snapshot
}

function Restore-SubtitlerEnvironment {
    param([Parameter(Mandatory)] [hashtable]$Snapshot)

    Get-ChildItem Env: |
        Where-Object { $_.Name -like 'SUBTITLER_*' } |
        ForEach-Object { Remove-Item -LiteralPath ("Env:" + $_.Name) -ErrorAction SilentlyContinue }
    foreach ($entry in $Snapshot.GetEnumerator()) {
        [Environment]::SetEnvironmentVariable($entry.Key, [string]$entry.Value, 'Process')
    }
}

function Assert-Condition {
    param(
        [Parameter(Mandatory)] [bool]$Condition,
        [Parameter(Mandatory)] [string]$Message
    )
    if (-not $Condition) {
        throw $Message
    }
}

function Assert-EquivalentEnvironment {
    param(
        [Parameter(Mandatory)] [hashtable]$Expected,
        [Parameter(Mandatory)] [hashtable]$Actual
    )

    Assert-Condition -Condition ($Expected.Count -eq $Actual.Count) -Message 'The real-media helper left a different number of SUBTITLER_* environment variables behind.'
    foreach ($entry in $Expected.GetEnumerator()) {
        Assert-Condition -Condition ($Actual.ContainsKey($entry.Key) -and $Actual[$entry.Key] -ceq $entry.Value) -Message 'The real-media helper did not restore a pre-existing SUBTITLER_* environment variable.'
    }
}

$projectRoot = Split-Path -Parent $PSScriptRoot
$helper = Join-Path $PSScriptRoot 'test-real-local-pipeline.ps1'
if (-not (Test-Path -LiteralPath $helper -PathType Leaf)) {
    throw 'The real-media smoke helper script is missing.'
}

$parseErrors = @()
[void][System.Management.Automation.Language.Parser]::ParseFile((Resolve-Path -LiteralPath $helper), [ref]$null, [ref]$parseErrors)
Assert-Condition -Condition ($parseErrors.Count -eq 0) -Message 'The real-media smoke helper has PowerShell parse errors.'

# The helper is intentionally local-only. This small static guard is not a
# substitute for code review, but it prevents accidental introduction of the
# common PowerShell network/download commands into the opt-in fixture path.
$helperSource = Get-Content -LiteralPath $helper -Raw -Encoding UTF8
foreach ($forbiddenCommand in @('Invoke-WebRequest', 'Invoke-RestMethod', 'Start-BitsTransfer', 'WebClient', 'curl.exe', 'wget.exe')) {
    Assert-Condition -Condition ($helperSource.IndexOf($forbiddenCommand, [StringComparison]::OrdinalIgnoreCase) -lt 0) -Message 'The real-media smoke helper must not contain a network/download command.'
}

$temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) ('subtitler-real-pipeline-contract-' + [guid]::NewGuid().ToString('N'))
$environmentBeforeTest = Get-SubtitlerEnvironmentSnapshot
try {
    New-Item -ItemType Directory -Path $temporaryRoot -ErrorAction Stop | Out-Null
    $ffmpeg = Join-Path $temporaryRoot 'ffmpeg.exe'
    $whisper = Join-Path $temporaryRoot 'whisper-cli.exe'
    $model = Join-Path $temporaryRoot 'ggml-small.bin'
    $media = Join-Path $temporaryRoot 'fixture.wav'
    foreach ($path in @($ffmpeg, $whisper, $model, $media)) {
        New-Item -ItemType File -Path $path -ErrorAction Stop | Out-Null
    }

    # A sentinel proves that the helper restores *all* existing SUBTITLER_*
    # values after ValidateOnly, including when invoked from a long-lived
    # PowerShell session rather than a fresh process.
    $env:SUBTITLER_REAL_PIPELINE_CONTRACT_SENTINEL = 'preserve-me'
    $environmentBeforeHelper = Get-SubtitlerEnvironmentSnapshot
    $validation = @(& $helper `
        -FfmpegPath $ffmpeg `
        -WhisperCliPath $whisper `
        -ModelPath $model `
        -MediaPath $media `
        -MediaDurationMs 1000 `
        -ValidateOnly)

    Assert-Condition -Condition ($validation.Count -eq 1) -Message 'ValidateOnly did not return exactly one validation result.'
    Assert-Condition -Condition ($validation[0].validated -eq $true -and $validation[0].execution -eq 'not started') -Message 'ValidateOnly did not remain non-executing.'
    $validationJson = $validation[0] | ConvertTo-Json -Compress
    Assert-Condition -Condition ($validationJson.IndexOf($temporaryRoot, [StringComparison]::OrdinalIgnoreCase) -lt 0) -Message 'ValidateOnly exposed a local fixture path in normal output.'
    Assert-EquivalentEnvironment -Expected $environmentBeforeHelper -Actual (Get-SubtitlerEnvironmentSnapshot)

    $uncRejected = $false
    $uncErrorMessage = ''
    try {
        & $helper `
            -FfmpegPath $ffmpeg `
            -WhisperCliPath $whisper `
            -ModelPath $model `
            -MediaPath '\\example.invalid\fixture.wav' `
            -MediaDurationMs 1000 `
            -ValidateOnly | Out-Null
    }
    catch {
        $uncRejected = $true
        $uncErrorMessage = $_.Exception.Message
    }
    Assert-Condition -Condition $uncRejected -Message 'ValidateOnly accepted a UNC media path.'
    Assert-Condition -Condition ($uncErrorMessage.IndexOf('example.invalid', [StringComparison]::OrdinalIgnoreCase) -lt 0) -Message 'ValidateOnly exposed a rejected UNC media path in error output.'

    $unboundedTimeoutRejected = $false
    try {
        & $helper `
            -FfmpegPath $ffmpeg `
            -WhisperCliPath $whisper `
            -ModelPath $model `
            -MediaPath $media `
            -MediaDurationMs 1000 `
            -TimeoutSeconds 14401 `
            -ValidateOnly | Out-Null
    }
    catch {
        $unboundedTimeoutRejected = $true
    }
    Assert-Condition -Condition $unboundedTimeoutRejected -Message 'ValidateOnly accepted an unbounded whole-run timeout.'

    Write-Host 'Real local-pipeline helper contract validation passed (no FFmpeg, whisper.cpp, native host, media decoding, or network access).'
}
finally {
    Restore-SubtitlerEnvironment -Snapshot $environmentBeforeTest
    if (Test-Path -LiteralPath $temporaryRoot -PathType Container) {
        Remove-Item -LiteralPath $temporaryRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}
