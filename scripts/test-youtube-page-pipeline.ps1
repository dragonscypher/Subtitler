[CmdletBinding()]
param(
    [string]$YoutubePageUrl = 'https://youtu.be/ESjPc7I5h_Q?si=yU3zYNYxwciVw63Y',
    [ValidateRange(1, 20)]
    [int]$TimeoutMinutes = 10
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Explicit, opt-in real-media smoke test. It drives the release native host
# through Chrome Native Messaging frames but does not require Chrome, print a
# signed media URL, or print transcript contents. It is intentionally not part
# of default CI because it fetches a public recording and uses local ASR.
function Assert-SupportedYoutubePage {
    param([Parameter(Mandatory)] [string]$Value)

    try { $uri = [uri]$Value } catch { throw 'A valid HTTPS YouTube recording page is required.' }
    if ($uri.Scheme -ne 'https' -or -not [string]::IsNullOrEmpty($uri.UserInfo)) {
        throw 'A credential-free HTTPS YouTube recording page is required.'
    }
    $hostName = $uri.DnsSafeHost.ToLowerInvariant()
    $segments = @($uri.AbsolutePath.Split('/', [StringSplitOptions]::RemoveEmptyEntries))
    $isYoutubeHost = $hostName -eq 'youtube.com' -or $hostName.EndsWith('.youtube.com')
    $isWatch = $isYoutubeHost -and $uri.AbsolutePath -eq '/watch' -and $uri.Query -match '(?:^|[?&])v=[^&]+'
    $isEmbedded = $isYoutubeHost -and $segments.Count -eq 2 -and $segments[0] -in @('embed', 'shorts')
    $isShort = $hostName -eq 'youtu.be' -and $segments.Count -eq 1
    if (-not ($isWatch -or $isEmbedded -or $isShort)) {
        throw 'Only a normal YouTube watch, embed, shorts, or youtu.be recording URL is supported.'
    }
    return $uri.AbsoluteUri
}

function Read-Exactly {
    param(
        [Parameter(Mandatory)] [System.IO.Stream]$Stream,
        [Parameter(Mandatory)] [byte[]]$Buffer
    )
    $offset = 0
    while ($offset -lt $Buffer.Length) {
        $read = $Stream.Read($Buffer, $offset, $Buffer.Length - $offset)
        if ($read -le 0) { throw 'The native host closed before completing a response.' }
        $offset += $read
    }
}

$safePageUrl = Assert-SupportedYoutubePage -Value $YoutubePageUrl
$projectRoot = Split-Path -Parent $PSScriptRoot
$nativeHost = Join-Path $projectRoot 'native\target\release\subtitler-native-host.exe'
if (-not (Test-Path -LiteralPath $nativeHost -PathType Leaf)) {
    throw 'Build the release native host before running this real-media smoke test.'
}

$startInfo = [System.Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = $nativeHost
$startInfo.UseShellExecute = $false
$startInfo.RedirectStandardInput = $true
$startInfo.RedirectStandardOutput = $true
$startInfo.RedirectStandardError = $true
$process = [System.Diagnostics.Process]::new()
$process.StartInfo = $startInfo
if (-not $process.Start()) { throw 'Could not start the release Subtitler native host.' }

$inputStream = $process.StandardInput.BaseStream
$outputStream = $process.StandardOutput.BaseStream

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
    if ($responseLength -gt 1MB) { throw 'The native host returned an overlarge message.' }
    $responseBody = New-Object byte[] $responseLength
    Read-Exactly -Stream $outputStream -Buffer $responseBody
    return [Text.Encoding]::UTF8.GetString($responseBody) | ConvertFrom-Json
}

try {
    $handshake = Send-NativeMessage @{
        request_id = 'youtube-page-smoke-handshake'
        command = 'handshake'
        protocol_version = 1
        extension_version = '0.1.0'
    }
    if ($handshake.response -ne 'handshake' -or -not $handshake.capabilities.local_asr_available -or -not $handshake.capabilities.ffmpeg_available) {
        throw 'The native host did not report an available local FFmpeg/ASR engine.'
    }

    $started = Send-NativeMessage @{
        request_id = 'youtube-page-smoke-start'
        command = 'start'
        job = @{
            client_job_id = [guid]::NewGuid().ToString()
            kind = 'full_transcript'
            media = @{
                source = @{ kind = 'page'; page_url = $safePageUrl }
                hints = @{ duration_ms = 258000 }
            }
            settings = @{
                force_generate_with_subtitler = $true
                processing_preference = 'local_only'
                speaker_diarization = $false
            }
        }
    }
    if ($started.response -ne 'job_started') {
        $safeCode = if ($started.code) { [string]$started.code } else { 'unknown' }
        $safeMessage = if ($started.message) { [string]$started.message } else { 'No safe host message was returned.' }
        throw "The native host did not accept the YouTube page source: ${safeCode}: $safeMessage"
    }

    $jobId = [string]$started.job.job_id
    $deadline = [DateTime]::UtcNow.AddMinutes($TimeoutMinutes)
    do {
        Start-Sleep -Seconds 2
        $status = Send-NativeMessage @{
            request_id = ('youtube-page-status-' + [guid]::NewGuid().ToString('N'))
            command = 'status'
            job_id = $jobId
        }
        $job = $status.job
        Write-Host ('state={0}; processed_ms={1}' -f $job.state, $job.progress.processed_ms)
    } while ($job.state -notin @('completed', 'failed', 'cancelled') -and [DateTime]::UtcNow -lt $deadline)

    if ($job.state -ne 'completed') {
        $message = if ($job.failure.message) { [string]$job.failure.message } else { 'No safe failure message was returned.' }
        throw "YouTube page smoke test did not complete: $($job.state): $message"
    }

    $page = Send-NativeMessage @{
        request_id = 'youtube-page-transcript-page'
        command = 'get_transcript_segments'
        job_id = $jobId
        limit = 1
    }
    if ($page.response -ne 'transcript_segments' -or @($page.segments).Count -lt 1) {
        throw 'The completed job did not provide any timestamped transcript segment.'
    }
    Write-Host 'YouTube page local ASR smoke test passed (timestamped transcript segment returned; content redacted).'
}
finally {
    $inputStream.Close()
    $process.WaitForExit(5000) | Out-Null
    $process.Dispose()
}
