[CmdletBinding()]
param(
    [Parameter(Mandatory)] [ValidateNotNullOrEmpty()] [string]$FfmpegPath,
    [Parameter(Mandatory)] [ValidateNotNullOrEmpty()] [string]$WhisperCliPath,
    [Parameter(Mandatory)] [ValidateNotNullOrEmpty()] [string]$ModelPath,
    [Parameter(Mandatory)] [ValidateNotNullOrEmpty()] [string]$MediaPath,
    # This is deliberately supplied by the developer rather than guessed from
    # a media filename. It makes the emitted real-time factor auditable and is
    # required for the bounded subtitle scheduler.
    [Parameter(Mandatory)] [ValidateRange(1, 86400000)] [long]$MediaDurationMs,
    [ValidateSet('tiny', 'base', 'small', 'medium', 'large_v3_turbo')]
    [string]$Model = 'small',
    [ValidateSet('f16', 'q5_0', 'q5_k_m', 'q8_0')]
    [string]$Quantization = 'f16',
    [ValidateSet('cpu', 'cuda', 'metal', 'vulkan')]
    [string]$Backend = 'cpu',
    [ValidateSet('transcript', 'subtitle')]
    [string]$JobKind = 'transcript',
    [ValidateRange(15, 14400)] [int]$TimeoutSeconds = 1800,
    [ValidateRange(100, 10000)] [int]$PollIntervalMilliseconds = 500,
    [ValidateRange(1, 300)] [int]$NativeMessageTimeoutSeconds = 30,
    [switch]$VerifySubtitleCues,
    [switch]$RequireSpeech,
    [switch]$KeepArtifacts,
    # Validates the local-only contract and restores the process environment
    # without starting FFmpeg, whisper.cpp, or the native host. This exists so
    # routine CI can test the script's safety boundary without real media.
    [switch]$ValidateOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($VerifySubtitleCues -and $JobKind -ne 'subtitle') {
    throw '-VerifySubtitleCues requires -JobKind subtitle so the generated subtitle workflow is exercised.'
}

function Resolve-LocalRegularFile {
    param(
        [Parameter(Mandatory)] [string]$Candidate,
        [Parameter(Mandatory)] [string]$Label
    )

    if ([string]::IsNullOrWhiteSpace($Candidate) -or $Candidate -match '[\x00-\x1f]') {
        throw "$Label must be a non-empty local file path."
    }
    # A real-media smoke run must never reach a network share, Win32 device
    # path, alternate data stream, or a provider other than FileSystem.
    if ($Candidate -match '^(?:\\\\|//)' -or $Candidate -notmatch '^[A-Za-z]:[\\/]') {
        throw "$Label must be an absolute file path on a local Windows drive."
    }

    try {
        $fullPath = [IO.Path]::GetFullPath($Candidate)
    }
    catch {
        throw "$Label is not a valid local file path."
    }

    if ($fullPath -notmatch '^[A-Za-z]:\\' -or $fullPath.Substring(2).Contains(':')) {
        throw "$Label must not use an alternate data stream or device path."
    }

    try {
        $item = Get-Item -LiteralPath $fullPath -Force -ErrorAction Stop
    }
    catch {
        throw "$Label must refer to an existing local file."
    }

    if ($item.PSProvider.Name -ne 'FileSystem' -or $item.PSIsContainer) {
        throw "$Label must refer to a regular local file."
    }
    if (([int]$item.Attributes -band [int][IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "$Label must not be a symbolic link, junction, or other reparse point."
    }
    try {
        Assert-NoReparsePathComponents -FullPath $item.FullName -Label $Label
    }
    catch {
        throw "$Label must remain accessible through a non-reparse local directory tree."
    }

    try {
        $drive = [IO.DriveInfo]::new($fullPath.Substring(0, 3))
    }
    catch {
        throw "$Label must be on a local Windows drive."
    }
    if ($drive.DriveType -notin @([IO.DriveType]::Fixed, [IO.DriveType]::Removable, [IO.DriveType]::Ram)) {
        throw "$Label must be on a fixed, removable, or RAM local drive; network and virtual paths are not allowed."
    }

    return $item.FullName
}

function Assert-NoReparsePathComponents {
    param(
        [Parameter(Mandatory)] [string]$FullPath,
        [Parameter(Mandatory)] [string]$Label
    )

    # Checking only the leaf is insufficient: C:\local\junction\fixture.wav
    # has a normal leaf while an ancestor can redirect it to another volume or
    # a network location. Walk the local ancestor chain before process launch.
    $volumeRoot = $FullPath.Substring(0, 3)
    $current = [IO.Path]::GetDirectoryName($FullPath)
    while (-not [string]::IsNullOrWhiteSpace($current)) {
        $ancestor = Get-Item -LiteralPath $current -Force -ErrorAction Stop
        if (([int]$ancestor.Attributes -band [int][IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "$Label must not be located beneath a symbolic link, junction, or other reparse point."
        }
        if ([string]::Equals($current.TrimEnd('\\'), $volumeRoot.TrimEnd('\\'), [StringComparison]::OrdinalIgnoreCase)) {
            return
        }
        $current = [IO.Path]::GetDirectoryName($current.TrimEnd('\\'))
    }
    throw "$Label must remain beneath a local Windows drive root."
}

function Get-SubtitlerEnvironmentSnapshot {
    $snapshot = @{}
    Get-ChildItem Env: |
        Where-Object { $_.Name -like 'SUBTITLER_*' } |
        ForEach-Object { $snapshot[$_.Name] = $_.Value }
    return $snapshot
}

function Restore-SubtitlerEnvironment {
    param([Parameter(Mandatory)] [hashtable]$Snapshot)

    # Remove every value added or changed by this helper, including a future
    # SUBTITLER_* variable it may set. Then restore exactly what the invoking
    # shell had before the run. This matters when a developer dot-sources the
    # script or uses it repeatedly in a long-lived terminal.
    Get-ChildItem Env: |
        Where-Object { $_.Name -like 'SUBTITLER_*' } |
        ForEach-Object { Remove-Item -LiteralPath ("Env:" + $_.Name) -ErrorAction SilentlyContinue }
    foreach ($entry in $Snapshot.GetEnumerator()) {
        [Environment]::SetEnvironmentVariable($entry.Key, [string]$entry.Value, 'Process')
    }
}

function Read-Exactly {
    param(
        [Parameter(Mandatory)] [System.IO.Stream]$Stream,
        [Parameter(Mandatory)] [byte[]]$Buffer,
        [Parameter(Mandatory)] [int]$MaximumTimeoutMilliseconds,
        [Parameter(Mandatory)] [DateTime]$Deadline
    )

    $offset = 0
    while ($offset -lt $Buffer.Length) {
        $remainingMilliseconds = Get-RemainingMilliseconds -Deadline $Deadline
        if ($remainingMilliseconds -lt 1) {
            throw 'The native host did not return a complete response before its configured deadline.'
        }
        $readTimeoutMilliseconds = [Math]::Min($MaximumTimeoutMilliseconds, $remainingMilliseconds)
        $readTask = $Stream.ReadAsync($Buffer, $offset, $Buffer.Length - $offset)
        if (-not $readTask.Wait($readTimeoutMilliseconds)) {
            throw 'The native host did not return a complete response before the configured message timeout.'
        }
        $received = $readTask.GetAwaiter().GetResult()
        if ($received -le 0) {
            throw 'The native host closed its output before a complete response frame was received.'
        }
        $offset += $received
    }
}

function Get-RemainingMilliseconds {
    param([Parameter(Mandatory)] [DateTime]$Deadline)

    $remaining = ($Deadline - [DateTime]::UtcNow).TotalMilliseconds
    if ($remaining -le 0) {
        return 0
    }
    return [int][Math]::Min(2147483647, [Math]::Ceiling($remaining))
}

function Send-NativeMessage {
    param(
        [Parameter(Mandatory)] [hashtable]$Message,
        [Parameter(Mandatory)] [System.IO.Stream]$InputStream,
        [Parameter(Mandatory)] [System.IO.Stream]$OutputStream,
        [Parameter(Mandatory)] [int]$MessageTimeoutMilliseconds,
        [Parameter(Mandatory)] [DateTime]$Deadline
    )

    $json = $Message | ConvertTo-Json -Depth 12 -Compress
    $payload = [Text.Encoding]::UTF8.GetBytes($json)
    if ($payload.Length -gt 1MB) {
        throw 'The real-media helper refused to send an overlarge Native Messaging request.'
    }
    $header = [BitConverter]::GetBytes([UInt32]$payload.Length)
    $InputStream.Write($header, 0, $header.Length)
    $InputStream.Write($payload, 0, $payload.Length)
    $InputStream.Flush()

    $messageDeadline = [DateTime]::UtcNow.AddMilliseconds($MessageTimeoutMilliseconds)
    $responseDeadline = if ($messageDeadline -lt $Deadline) { $messageDeadline } else { $Deadline }
    $responseHeader = New-Object byte[] 4
    Read-Exactly -Stream $OutputStream -Buffer $responseHeader -MaximumTimeoutMilliseconds $MessageTimeoutMilliseconds -Deadline $responseDeadline
    $responseLength = [BitConverter]::ToUInt32($responseHeader, 0)
    if ($responseLength -gt 1MB) {
        throw 'The native host returned an overlarge response.'
    }
    $responseBody = New-Object byte[] $responseLength
    Read-Exactly -Stream $OutputStream -Buffer $responseBody -MaximumTimeoutMilliseconds $MessageTimeoutMilliseconds -Deadline $responseDeadline
    try {
        return [Text.Encoding]::UTF8.GetString($responseBody) | ConvertFrom-Json -ErrorAction Stop
    }
    catch {
        throw 'The native host returned an invalid JSON response.'
    }
}

function Get-UnsignedMilliseconds {
    param(
        [Parameter(Mandatory)] $Value,
        [Parameter(Mandatory)] [string]$Label
    )

    [UInt64]$parsed = 0
    if (-not [UInt64]::TryParse([string]$Value, [ref]$parsed)) {
        throw "$Label must be a non-negative whole number of milliseconds."
    }
    return $parsed
}

function Test-TranscriptExport {
    param(
        [Parameter(Mandatory)] [string]$TranscriptJsonPath,
        [Parameter(Mandatory)] [bool]$SpeechRequired
    )

    try {
        $export = Get-Item -LiteralPath $TranscriptJsonPath -Force -ErrorAction Stop
    }
    catch {
        throw 'Subtitler could not read its private transcript export.'
    }
    if ($export.Length -gt 16MB) {
        throw 'Transcript.json exceeded the real-media smoke helper safety limit.'
    }
    try {
        $transcript = Get-Content -LiteralPath $TranscriptJsonPath -Raw -Encoding UTF8 | ConvertFrom-Json -ErrorAction Stop
    }
    catch {
        throw 'Transcript.json was not valid UTF-8 JSON.'
    }
    if ($null -eq $transcript.segments) {
        throw 'Transcript.json did not contain transcript segments.'
    }

    [UInt64]$previousStart = 0
    $segmentCount = 0
    foreach ($segment in @($transcript.segments)) {
        if ($null -eq $segment.timing -or [string]::IsNullOrWhiteSpace([string]$segment.text)) {
            throw 'Transcript.json contained an invalid display segment.'
        }
        $startMs = Get-UnsignedMilliseconds -Value $segment.timing.start_ms -Label 'Transcript segment start timestamp'
        $endMs = Get-UnsignedMilliseconds -Value $segment.timing.end_ms -Label 'Transcript segment end timestamp'
        if ($endMs -lt $startMs -or ($segmentCount -gt 0 -and $startMs -lt $previousStart)) {
            throw 'Transcript.json contained non-monotonic timestamps.'
        }
        $previousStart = $startMs
        $segmentCount += 1
    }
    if ($SpeechRequired -and $segmentCount -lt 1) {
        throw 'The supplied speech fixture produced no transcript segments.'
    }
    return $segmentCount
}

function Remove-OwnedTemporaryDirectory {
    param(
        [Parameter(Mandatory)] [string]$TemporaryRoot,
        [Parameter(Mandatory)] [string]$TemporaryParent
    )

    if (-not (Test-Path -LiteralPath $TemporaryRoot -PathType Container)) {
        return
    }
    $resolvedRoot = [IO.Path]::GetFullPath($TemporaryRoot).TrimEnd('\\')
    $resolvedParent = [IO.Path]::GetFullPath($TemporaryParent).TrimEnd('\\')
    $actualParent = [IO.Path]::GetDirectoryName($resolvedRoot).TrimEnd('\\')
    $leaf = Split-Path -Leaf $resolvedRoot
    if (
        -not [string]::Equals($actualParent, $resolvedParent, [StringComparison]::OrdinalIgnoreCase) -or
        $leaf -notmatch '^subtitler-real-pipeline-[0-9a-f]{32}$'
    ) {
        throw 'The real-media helper refused to clean an unexpected temporary directory.'
    }
    $item = Get-Item -LiteralPath $resolvedRoot -Force -ErrorAction Stop
    if (([int]$item.Attributes -band [int][IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw 'The real-media helper refused to recurse into a temporary reparse point.'
    }
    Assert-NoReparseDescendants -Directory $resolvedRoot
    Remove-Item -LiteralPath $resolvedRoot -Recurse -Force -ErrorAction Stop
}

function Assert-NoReparseDescendants {
    param([Parameter(Mandatory)] [string]$Directory)

    # Do not rely on recursive PowerShell enumeration for cleanup. A tool
    # failure or a hostile local process must not turn a private temporary
    # directory into a junction that causes recursive deletion elsewhere.
    $pending = [System.Collections.Generic.Stack[string]]::new()
    $pending.Push($Directory)
    while ($pending.Count -gt 0) {
        $current = $pending.Pop()
        foreach ($entry in [IO.Directory]::EnumerateFileSystemEntries($current)) {
            $attributes = [IO.File]::GetAttributes($entry)
            if (([int]$attributes -band [int][IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw 'The real-media helper refused to recurse through a temporary reparse point.'
            }
            if (($attributes -band [IO.FileAttributes]::Directory) -ne 0) {
                $pending.Push($entry)
            }
        }
    }
}

$projectRoot = Split-Path -Parent $PSScriptRoot
$nativeHost = Join-Path $projectRoot 'native\target\release\subtitler-native-host.exe'
$validatedFfmpegPath = Resolve-LocalRegularFile -Candidate $FfmpegPath -Label 'FFmpeg executable'
$validatedWhisperCliPath = Resolve-LocalRegularFile -Candidate $WhisperCliPath -Label 'whisper.cpp executable'
$validatedModelPath = Resolve-LocalRegularFile -Candidate $ModelPath -Label 'whisper.cpp model'
$validatedMediaPath = Resolve-LocalRegularFile -Candidate $MediaPath -Label 'media fixture'

if (-not $ValidateOnly) {
    $nativeHost = Resolve-LocalRegularFile -Candidate $nativeHost -Label 'compiled Subtitler native host'
}

$environmentSnapshot = Get-SubtitlerEnvironmentSnapshot
$temporaryParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$temporaryRoot = $null
$process = $null
$inputStream = $null
$outputStream = $null
$artifactsRetained = $false

try {
    $env:SUBTITLER_FFMPEG_PATH = $validatedFfmpegPath
    $env:SUBTITLER_WHISPER_CPP_PATH = $validatedWhisperCliPath
    $env:SUBTITLER_WHISPER_MODEL_PATH = $validatedModelPath
    $env:SUBTITLER_LOCAL_MODEL = $Model
    $env:SUBTITLER_MODEL_QUANTIZATION = $Quantization
    $env:SUBTITLER_COMPUTE_BACKEND = $Backend

    if ($ValidateOnly) {
        [pscustomobject]@{
            validated = $true
            execution = 'not started'
            mediaSource = 'local_file only'
            networkAccess = 'not used'
            mediaDurationMs = $MediaDurationMs
        }
        return
    }

    $temporaryRoot = Join-Path $temporaryParent ('subtitler-real-pipeline-' + [guid]::NewGuid().ToString('N'))
    try {
        New-Item -ItemType Directory -Path $temporaryRoot -ErrorAction Stop | Out-Null
    }
    catch {
        throw 'Subtitler could not create its private temporary processing directory.'
    }
    $cacheRoot = Join-Path $temporaryRoot 'cache'
    $exportRoot = Join-Path $temporaryRoot 'exports'
    $env:SUBTITLER_CACHE_ROOT = $cacheRoot
    $env:SUBTITLER_EXPORT_ROOT = $exportRoot

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $nativeHost
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    # Keep the protocol stream bounded and free of diagnostic text. The host
    # intentionally redacts input paths from routine errors, and stderr is
    # inherited rather than accumulating in an unread pipe.
    $startInfo.RedirectStandardError = $false

    $runStarted = [DateTime]::UtcNow
    $runStopwatch = [Diagnostics.Stopwatch]::StartNew()
    $deadline = $runStarted.AddSeconds($TimeoutSeconds)
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    try {
        $nativeHostStarted = $process.Start()
    }
    catch {
        throw 'Could not launch the configured Subtitler native host.'
    }
    if (-not $nativeHostStarted) {
        throw 'Could not launch the configured Subtitler native host.'
    }
    $inputStream = $process.StandardInput.BaseStream
    $outputStream = $process.StandardOutput.BaseStream
    $messageTimeoutMilliseconds = $NativeMessageTimeoutSeconds * 1000

    $handshake = Send-NativeMessage -InputStream $inputStream -OutputStream $outputStream -MessageTimeoutMilliseconds $messageTimeoutMilliseconds -Deadline $deadline -Message @{
        request_id = 'real-handshake'
        command = 'handshake'
        protocol_version = 1
        extension_version = '0.1.0'
    }
    if ($handshake.response -ne 'handshake' -or -not $handshake.capabilities.local_asr_available -or -not $handshake.capabilities.ffmpeg_available) {
        throw 'The configured native host did not advertise local FFmpeg and ASR capabilities.'
    }

    $nativeJobKind = if ($JobKind -eq 'subtitle') { 'subtitle_generation' } else { 'full_transcript' }
    $started = Send-NativeMessage -InputStream $inputStream -OutputStream $outputStream -MessageTimeoutMilliseconds $messageTimeoutMilliseconds -Deadline $deadline -Message @{
        request_id = 'real-start'
        command = 'start'
        job = @{
            client_job_id = [guid]::NewGuid().ToString()
            kind = $nativeJobKind
            media = @{
                source = @{
                    kind = 'local_file'
                    path = $validatedMediaPath
                }
                hints = @{ duration_ms = [UInt64]$MediaDurationMs }
            }
        }
    }
    if ($started.response -ne 'job_started' -or [string]::IsNullOrWhiteSpace([string]$started.job.job_id)) {
        throw 'The configured native host did not accept the local media job.'
    }

    $jobId = [string]$started.job.job_id
    if ($JobKind -eq 'subtitle') {
        # There is no browser/media clock in this local-only harness. Moving
        # the synthetic playhead to the supplied end time lets the scheduler
        # finish every uncovered range and produce final exports. It is not a
        # replay or proof that the product can remain ahead of real playback.
        $playback = Send-NativeMessage -InputStream $inputStream -OutputStream $outputStream -MessageTimeoutMilliseconds $messageTimeoutMilliseconds -Deadline $deadline -Message @{
            request_id = 'real-subtitle-complete'
            command = 'playback_update'
            job_id = $jobId
            position_ms = [UInt64]$MediaDurationMs
            playback_rate_milli = 1000
            is_paused = $true
            seek_generation = 1
        }
        if ($playback.response -ne 'job_status' -or $playback.job.job_id -ne $jobId) {
            throw 'The subtitle smoke job did not accept its synthetic completion playhead.'
        }
    }

    $status = $null
    do {
        $remainingMilliseconds = Get-RemainingMilliseconds -Deadline $deadline
        if ($remainingMilliseconds -lt 1) {
            throw "The real local pipeline did not complete within the configured $TimeoutSeconds-second limit."
        }
        Start-Sleep -Milliseconds ([Math]::Min($PollIntervalMilliseconds, $remainingMilliseconds))
        $status = Send-NativeMessage -InputStream $inputStream -OutputStream $outputStream -MessageTimeoutMilliseconds $messageTimeoutMilliseconds -Deadline $deadline -Message @{
            request_id = 'real-status'
            command = 'status'
            job_id = $jobId
        }
        if ($status.response -ne 'job_status' -or $status.job.job_id -ne $jobId) {
            throw 'The native host returned an invalid job-status response.'
        }
    } while ($status.job.state -notin @('completed', 'failed', 'cancelled'))

    if ($status.job.state -ne 'completed') {
        throw "The real local pipeline did not complete: $($status.job.state)."
    }

    $jobExports = Join-Path $exportRoot $jobId
    $requiredFiles = @('Transcript.txt', 'Transcript-timestamped.txt', 'Subtitles.srt', 'Subtitles.vtt', 'Transcript.json')
    $missing = $requiredFiles | Where-Object { -not (Test-Path -LiteralPath (Join-Path $jobExports $_) -PathType Leaf) }
    if ($missing) {
        throw 'The real local pipeline did not produce every required export.'
    }
    $transcriptSegmentCount = Test-TranscriptExport -TranscriptJsonPath (Join-Path $jobExports 'Transcript.json') -SpeechRequired $RequireSpeech.IsPresent

    $cuePages = 0
    $cueCount = 0
    if ($VerifySubtitleCues) {
        $cursor = $null
        do {
            $cueRequest = @{
                request_id = "real-cues-$cuePages"
                command = 'get_subtitle_cues'
                job_id = $jobId
                limit = 200
            }
            if ($null -ne $cursor) {
                $cueRequest.cursor = $cursor
            }
            $cueResponse = Send-NativeMessage -InputStream $inputStream -OutputStream $outputStream -MessageTimeoutMilliseconds $messageTimeoutMilliseconds -Deadline $deadline -Message $cueRequest
            if ($cueResponse.response -ne 'subtitle_cues' -or $cueResponse.job_id -ne $jobId) {
                throw 'The completed subtitle job did not return a valid subtitle-cue page.'
            }
            foreach ($cue in @($cueResponse.cues)) {
                if ($null -eq $cue.timing -or $cue.timing.end_ms -le $cue.timing.start_ms -or @($cue.lines).Count -lt 1) {
                    throw 'The native host returned an invalid generated subtitle cue.'
                }
            }
            $cueCount += @($cueResponse.cues).Count
            $cuePages += 1
            # `next_cursor` is intentionally omitted on the final native page.
            # Under StrictMode, direct property access would turn that valid
            # terminal response into a harness failure.
            $nextCursorProperty = $cueResponse.PSObject.Properties['next_cursor']
            $cursor = if ($null -eq $nextCursorProperty) { $null } else { $nextCursorProperty.Value }
        } while ($null -ne $cursor)

        if ($cueCount -lt 1) {
            throw 'The completed subtitle job returned no generated cues for the supplied speech fixture.'
        }
    }

    $runStopwatch.Stop()
    $elapsedMilliseconds = [int64]$runStopwatch.ElapsedMilliseconds
    if ($elapsedMilliseconds -lt 1) {
        $elapsedMilliseconds = 1
    }
    $realTimeFactor = [Math]::Round($elapsedMilliseconds / [double]$MediaDurationMs, 3)
    $result = [ordered]@{
        state = $status.job.state
        jobKind = $nativeJobKind
        exportsVerified = $requiredFiles.Count
        transcriptSegments = $transcriptSegmentCount
        subtitleCuePages = $cuePages
        subtitleCues = $cueCount
        declaredMediaDurationMs = $MediaDurationMs
        wallElapsedMs = $elapsedMilliseconds
        endToEndRealTimeFactor = $realTimeFactor
        performanceEvidence = 'single local smoke measurement; not a production benchmark or ahead-of-playback claim'
        temporaryArtifactsRetained = $KeepArtifacts.IsPresent
    }
    if ($KeepArtifacts) {
        # A developer explicitly requested retention, so return the location
        # only in that opt-in case. It is never persisted or sent elsewhere.
        $result['temporaryArtifactDirectory'] = $temporaryRoot
        $artifactsRetained = $true
    }
    [pscustomobject]$result
}
finally {
    if ($null -ne $process) {
        if ($null -ne $inputStream) {
            $process.StandardInput.Close()
        }
        if (-not $process.HasExited) {
            $process.WaitForExit(5000) | Out-Null
        }
        if (-not $process.HasExited) {
            $process.Kill()
            $process.WaitForExit(5000) | Out-Null
        }
        $process.Dispose()
    }

    if ($null -ne $temporaryRoot -and -not $artifactsRetained) {
        try {
            Remove-OwnedTemporaryDirectory -TemporaryRoot $temporaryRoot -TemporaryParent $temporaryParent
        }
        catch {
            Write-Warning 'Subtitler could not safely remove its private real-media smoke-test artifacts.'
        }
    }
    Restore-SubtitlerEnvironment -Snapshot $environmentSnapshot
}
