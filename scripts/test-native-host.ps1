[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$projectRoot = Split-Path -Parent $PSScriptRoot
$nativeManifest = Join-Path $projectRoot 'native\Cargo.toml'
$cargoPath = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
if (-not (Test-Path -LiteralPath $cargoPath)) {
    $cargoCommand = Get-Command cargo -ErrorAction Stop
    $cargoPath = $cargoCommand.Source
}

& $cargoPath build --manifest-path $nativeManifest -p subtitler-native-host --locked
if ($LASTEXITCODE -ne 0) {
    throw "Could not build subtitler-native-host (exit code $LASTEXITCODE)."
}

$nativeHostExe = Join-Path $projectRoot 'native\target\debug\subtitler-native-host.exe'
if (-not (Test-Path -LiteralPath $nativeHostExe)) {
    throw "The compiled native host was not found at $nativeHostExe."
}

$startInfo = [System.Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = $nativeHostExe
$startInfo.UseShellExecute = $false
$startInfo.RedirectStandardInput = $true
$startInfo.RedirectStandardOutput = $true
$startInfo.RedirectStandardError = $true
# This smoke verifies the unavailable-engine response deterministically. The
# override applies only to the launched child; it never changes the user's
# environment or any installed Subtitler asset.
$startInfo.Environment['SUBTITLER_WHISPER_MODEL_PATH'] = (Join-Path ([IO.Path]::GetTempPath()) 'subtitler-smoke-missing-model.bin')

$nativeHostProcess = [System.Diagnostics.Process]::new()
$nativeHostProcess.StartInfo = $startInfo
if (-not $nativeHostProcess.Start()) {
    throw 'Could not start the Subtitler native host.'
}

$inputStream = $nativeHostProcess.StandardInput.BaseStream
$outputStream = $nativeHostProcess.StandardOutput.BaseStream

function Read-Exactly {
    param(
        [Parameter(Mandatory)] [System.IO.Stream]$Stream,
        [Parameter(Mandatory)] [byte[]]$Buffer
    )

    $offset = 0
    while ($offset -lt $Buffer.Length) {
        $received = $Stream.Read($Buffer, $offset, $Buffer.Length - $offset)
        if ($received -le 0) {
            throw 'The native host closed its output before a complete message was received.'
        }
        $offset += $received
    }
}

function Send-NativeMessage {
    param([Parameter(Mandatory)] [hashtable]$Message)

    $json = $Message | ConvertTo-Json -Depth 12 -Compress
    $payload = [Text.Encoding]::UTF8.GetBytes($json)
    $header = [BitConverter]::GetBytes([UInt32]$payload.Length)
    $inputStream.Write($header, 0, $header.Length)
    $inputStream.Write($payload, 0, $payload.Length)
    $inputStream.Flush()

    $responseHeader = New-Object byte[] 4
    Read-Exactly -Stream $outputStream -Buffer $responseHeader
    $responseLength = [BitConverter]::ToUInt32($responseHeader, 0)
    if ($responseLength -gt 1MB) {
        throw "The native host returned an overlarge response ($responseLength bytes)."
    }
    $responseBody = New-Object byte[] $responseLength
    Read-Exactly -Stream $outputStream -Buffer $responseBody
    return [Text.Encoding]::UTF8.GetString($responseBody) | ConvertFrom-Json
}

try {
    $handshake = Send-NativeMessage @{
        request_id = 'smoke-handshake'
        command = 'handshake'
        protocol_version = 1
        extension_version = '0.1.0'
    }
    if ($handshake.response -ne 'handshake' -or $handshake.native_host_name -ne 'com.subtitler.native_host' -or $handshake.protocol_version -ne 1) {
        throw "Unexpected handshake response: $($handshake | ConvertTo-Json -Compress)"
    }

    # This smoke launches with a child-only missing model override. The host
    # must reject a generated job clearly instead of accepting work that it
    # cannot process. Full lifecycle behavior is covered by the Rust dispatcher
    # tests with an injected local runner.
    $clientJobId = [guid]::NewGuid().ToString()
    $unavailable = Send-NativeMessage @{
        request_id = 'smoke-start'
        command = 'start'
        job = @{
            client_job_id = $clientJobId
            kind = 'full_transcript'
            media = @{
                source = @{
                    kind = 'direct_url'
                    media_url = 'https://media.example.test/recording.mp4'
                }
                hints = @{ duration_ms = 60000 }
            }
        }
    }
    if ($unavailable.response -ne 'error' -or $unavailable.code -ne 'engine_unavailable' -or -not $unavailable.retryable) {
        throw "Unexpected unavailable-engine response: $($unavailable | ConvertTo-Json -Depth 12 -Compress)"
    }

    Write-Host 'Native Messaging handshake/unavailable-engine smoke test passed.'
}
finally {
    $nativeHostProcess.StandardInput.Close()
    if (-not $nativeHostProcess.HasExited) {
        $nativeHostProcess.WaitForExit(5000) | Out-Null
    }
    if (-not $nativeHostProcess.HasExited) {
        $nativeHostProcess.Kill()
    }
    $nativeHostProcess.Dispose()
}
