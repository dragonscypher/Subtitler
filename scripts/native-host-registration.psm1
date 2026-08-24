Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# This module intentionally contains no extension-controlled input and no
# process launching.  It is shared by the developer-only register, unregister,
# and validation scripts so their security checks cannot drift apart.
$script:SubtitlerNativeHostName = 'com.subtitler.native_host'
$script:SubtitlerNativeHostDescription = 'Subtitler Native Messaging Host (development)'
$script:SubtitlerNativeHostRegistryPath = 'HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.subtitler.native_host'
$script:MaximumManifestBytes = 64KB
$script:ExpectedManifestProperties = @(
    'name',
    'description',
    'path',
    'type',
    'allowed_origins'
)

function Get-SubtitlerNativeHostName {
    [OutputType([string])]
    param()

    return $script:SubtitlerNativeHostName
}

function Get-SubtitlerNativeHostRegistryPath {
    [OutputType([string])]
    param()

    return $script:SubtitlerNativeHostRegistryPath
}

function Test-SubtitlerPathEqual {
    [OutputType([bool])]
    param(
        [Parameter(Mandatory)]
        [string]$Left,

        [Parameter(Mandatory)]
        [string]$Right
    )

    return [string]::Equals(
        $Left,
        $Right,
        [System.StringComparison]::OrdinalIgnoreCase
    )
}

function Assert-SubtitlerExtensionId {
    [OutputType([string])]
    param(
        [Parameter(Mandatory)]
        [string]$ExtensionId
    )

    # -cmatch deliberately makes upper-case characters invalid. Chrome IDs are
    # lower-case a-p, and accepting a differently cased value would produce a
    # misleading origin allowlist.
    if ($ExtensionId -cnotmatch '^[a-p]{32}$') {
        throw 'ExtensionId must be the exact 32-character lower-case Chrome extension ID (a-p only).'
    }

    return $ExtensionId
}

function Get-SubtitlerAllowedOrigin {
    [OutputType([string])]
    param(
        [Parameter(Mandatory)]
        [string]$ExtensionId
    )

    $validatedExtensionId = Assert-SubtitlerExtensionId -ExtensionId $ExtensionId
    return "chrome-extension://$validatedExtensionId/"
}

function Assert-SubtitlerNoReparsePointInExistingPath {
    param(
        [Parameter(Mandatory)]
        [string]$FullPath
    )

    $probe = $FullPath
    while ($true) {
        $item = Get-Item -LiteralPath $probe -Force -ErrorAction SilentlyContinue
        if ($null -ne $item) {
            $attributes = [System.IO.FileAttributes]$item.Attributes
            if (($attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Reparse points are not permitted in a developer native-host path: $probe"
            }
        }

        $parent = [System.IO.Directory]::GetParent($probe)
        if ($null -eq $parent -or $parent.FullName -ceq $probe) {
            break
        }

        $probe = $parent.FullName
    }
}

function Resolve-SubtitlerLocalPath {
    [OutputType([string])]
    param(
        [Parameter(Mandatory)]
        [string]$Path,

        [ValidateSet('Any', 'Leaf', 'Container')]
        [string]$ExpectedType = 'Any',

        [switch]$MustExist
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        throw 'A path is required.'
    }

    if ($Path -cne $Path.Trim()) {
        throw 'Paths with leading or trailing whitespace are not accepted.'
    }

    if ($Path.IndexOf([char]0) -ge 0 -or $Path -match '[\x01-\x1F]') {
        throw 'Paths containing NUL or control characters are not accepted.'
    }

    # Do not treat UNC, device, or NT-object-manager paths as local paths.
    # A mapped network share can look like a drive path, so DriveInfo is also
    # checked below rather than relying on string prefixes alone.
    $disallowedPrefixes = @('\\', '//', '\\?\', '\\.\', '\??\', '\.\', '\Device\')
    foreach ($prefix in $disallowedPrefixes) {
        if ($Path.StartsWith($prefix, [System.StringComparison]::Ordinal)) {
            throw "UNC, device, and NT-object-manager paths are not permitted: $Path"
        }
    }

    if ($Path -notmatch '^[A-Za-z]:[\\/]') {
        throw "Path must be an absolute local drive path (for example C:\Subtitler): $Path"
    }

    $fullPath = [System.IO.Path]::GetFullPath($Path)
    if ($fullPath -notmatch '^[A-Za-z]:\\') {
        throw "Path is not a fully qualified local drive path: $Path"
    }

    if ($fullPath.Length -gt 2 -and $fullPath.Substring(2).Contains(':')) {
        throw 'Alternate data streams and additional colon path components are not permitted.'
    }

    $relativeComponents = $fullPath.Substring(3).Split([char]'\')
    foreach ($component in $relativeComponents) {
        if ($component.EndsWith('.') -or $component.EndsWith(' ')) {
            throw 'Paths with Windows-normalized trailing dots or spaces are not permitted.'
        }
    }

    $root = [System.IO.Path]::GetPathRoot($fullPath)
    if ($root -notmatch '^[A-Za-z]:\\$') {
        throw "Path has an unsupported root: $Path"
    }

    try {
        $drive = [System.IO.DriveInfo]::new($root)
    }
    catch {
        throw "Could not inspect the path drive for $Path."
    }

    if (-not $drive.IsReady -or $drive.DriveType -ne [System.IO.DriveType]::Fixed) {
        throw "Path must be on a ready fixed local drive, not a network, removable, or virtual location: $Path"
    }

    $existingItem = Get-Item -LiteralPath $fullPath -Force -ErrorAction SilentlyContinue
    if ($MustExist -and $null -eq $existingItem) {
        throw "Required path does not exist: $fullPath"
    }

    if ($null -ne $existingItem) {
        if ($ExpectedType -eq 'Leaf' -and $existingItem.PSIsContainer) {
            throw "Expected a file but found a directory: $fullPath"
        }

        if ($ExpectedType -eq 'Container' -and -not $existingItem.PSIsContainer) {
            throw "Expected a directory but found a file: $fullPath"
        }
    }

    Assert-SubtitlerNoReparsePointInExistingPath -FullPath $fullPath
    return $fullPath
}

function Get-SubtitlerDeveloperInstallRoot {
    [OutputType([string])]
    param()

    $localAppData = [System.Environment]::GetFolderPath(
        [System.Environment+SpecialFolder]::LocalApplicationData
    )
    if ([string]::IsNullOrWhiteSpace($localAppData)) {
        throw 'Could not determine the current user LocalAppData directory.'
    }

    return Resolve-SubtitlerLocalPath -Path (Join-Path $localAppData 'Subtitler\developer') -ExpectedType Container
}

function Resolve-SubtitlerDeveloperInstallDirectory {
    [OutputType([string])]
    param(
        [Parameter(Mandatory)]
        [string]$InstallDirectory
    )

    $developerRoot = Get-SubtitlerDeveloperInstallRoot
    $resolvedInstallDirectory = Resolve-SubtitlerLocalPath -Path $InstallDirectory -ExpectedType Container
    $developerRootWithSeparator = $developerRoot.TrimEnd([char]'\') + '\'

    $isDeveloperRoot = Test-SubtitlerPathEqual -Left $resolvedInstallDirectory -Right $developerRoot
    $isDeveloperChild = $resolvedInstallDirectory.StartsWith(
        $developerRootWithSeparator,
        [System.StringComparison]::OrdinalIgnoreCase
    )
    if ($isDeveloperRoot -or -not $isDeveloperChild) {
        throw "InstallDirectory must be a child of the developer-owned directory: $developerRoot"
    }

    return $resolvedInstallDirectory
}

function Resolve-SubtitlerHostExecutable {
    [OutputType([string])]
    param(
        [Parameter(Mandatory)]
        [string]$HostExecutable,

        [switch]$MustExist
    )

    $resolvedExecutable = Resolve-SubtitlerLocalPath -Path $HostExecutable -ExpectedType Leaf -MustExist:$MustExist
    if ([System.IO.Path]::GetExtension($resolvedExecutable) -cne '.exe') {
        throw "HostExecutable must be an .exe file: $resolvedExecutable"
    }

    return $resolvedExecutable
}

function Get-SubtitlerNativeHostManifestPath {
    [OutputType([string])]
    param(
        [Parameter(Mandatory)]
        [string]$InstallDirectory
    )

    $resolvedInstallDirectory = Resolve-SubtitlerDeveloperInstallDirectory -InstallDirectory $InstallDirectory
    return Join-Path $resolvedInstallDirectory ("$script:SubtitlerNativeHostName.json")
}

function New-SubtitlerNativeHostManifest {
    [OutputType([System.Collections.Specialized.OrderedDictionary])]
    param(
        [Parameter(Mandatory)]
        [string]$ExtensionId,

        [Parameter(Mandatory)]
        [string]$HostExecutable
    )

    $validatedExtensionId = Assert-SubtitlerExtensionId -ExtensionId $ExtensionId
    $resolvedExecutable = Resolve-SubtitlerHostExecutable -HostExecutable $HostExecutable -MustExist

    return [ordered]@{
        name            = $script:SubtitlerNativeHostName
        description     = $script:SubtitlerNativeHostDescription
        path            = $resolvedExecutable
        type            = 'stdio'
        allowed_origins = @((Get-SubtitlerAllowedOrigin -ExtensionId $validatedExtensionId))
    }
}

function ConvertTo-SubtitlerNativeHostManifestJson {
    [OutputType([string])]
    param(
        [Parameter(Mandatory)]
        [System.Collections.IDictionary]$Manifest
    )

    return $Manifest | ConvertTo-Json -Depth 4
}

function Read-SubtitlerNativeHostManifest {
    [OutputType([pscustomobject])]
    param(
        [Parameter(Mandatory)]
        [string]$ManifestPath
    )

    $resolvedManifestPath = Resolve-SubtitlerLocalPath -Path $ManifestPath -ExpectedType Leaf -MustExist
    $manifestInfo = Get-Item -LiteralPath $resolvedManifestPath -Force
    if ($manifestInfo.Length -gt $script:MaximumManifestBytes) {
        throw "Native-host manifest is unexpectedly large ($($manifestInfo.Length) bytes)."
    }

    try {
        $json = [System.IO.File]::ReadAllText($resolvedManifestPath, [System.Text.Encoding]::UTF8)
        return $json | ConvertFrom-Json -ErrorAction Stop
    }
    catch {
        throw "Could not parse the native-host manifest at $resolvedManifestPath."
    }
}

function Test-SubtitlerNativeHostManifest {
    [OutputType([pscustomobject])]
    param(
        [Parameter(Mandatory)]
        [string]$ManifestPath,

        [Parameter(Mandatory)]
        [string]$ExtensionId,

        [string]$ExpectedHostExecutable,

        [switch]$RequireHostExecutable
    )

    $validatedExtensionId = Assert-SubtitlerExtensionId -ExtensionId $ExtensionId
    $resolvedManifestPath = Resolve-SubtitlerLocalPath -Path $ManifestPath -ExpectedType Leaf -MustExist
    $manifest = Read-SubtitlerNativeHostManifest -ManifestPath $resolvedManifestPath
    $propertyNames = @($manifest.PSObject.Properties | ForEach-Object { $_.Name })

    $hasUnexpectedProperty = @(
        $propertyNames | Where-Object { $_ -notin $script:ExpectedManifestProperties }
    ).Count -ne 0
    if ($propertyNames.Count -ne $script:ExpectedManifestProperties.Count -or $hasUnexpectedProperty) {
        throw 'Native-host manifest has an unexpected property set.'
    }

    foreach ($propertyName in $script:ExpectedManifestProperties) {
        if ($propertyName -notin $propertyNames) {
            throw "Native-host manifest is missing required property '$propertyName'."
        }
    }

    if ($manifest.name -cne $script:SubtitlerNativeHostName) {
        throw 'Native-host manifest name is not the exact Subtitler native-host name.'
    }

    if ($manifest.description -cne $script:SubtitlerNativeHostDescription) {
        throw 'Native-host manifest description is not the expected developer description.'
    }

    if ($manifest.type -cne 'stdio') {
        throw 'Native-host manifest type must be exactly stdio.'
    }

    if ($manifest.path -isnot [string]) {
        throw 'Native-host manifest path must be a string.'
    }

    $resolvedManifestExecutable = Resolve-SubtitlerHostExecutable -HostExecutable $manifest.path -MustExist:$RequireHostExecutable
    if (-not [string]::IsNullOrWhiteSpace($ExpectedHostExecutable)) {
        $resolvedExpectedExecutable = Resolve-SubtitlerHostExecutable -HostExecutable $ExpectedHostExecutable -MustExist:$RequireHostExecutable
        if (-not (Test-SubtitlerPathEqual -Left $resolvedManifestExecutable -Right $resolvedExpectedExecutable)) {
            throw 'Native-host manifest executable path does not match the expected executable.'
        }
    }

    if ($manifest.allowed_origins -is [string] -or $null -eq $manifest.allowed_origins) {
        throw 'Native-host manifest allowed_origins must be an array containing one exact origin.'
    }

    $allowedOrigins = @($manifest.allowed_origins)
    $expectedOrigin = Get-SubtitlerAllowedOrigin -ExtensionId $validatedExtensionId
    if ($allowedOrigins.Count -ne 1 -or $allowedOrigins[0] -cne $expectedOrigin) {
        throw 'Native-host manifest allowed_origins must contain only the exact requested Chrome extension origin.'
    }

    return [pscustomobject]@{
        ManifestPath   = $resolvedManifestPath
        HostExecutable = $resolvedManifestExecutable
        ExtensionId    = $validatedExtensionId
    }
}

function Set-SubtitlerPrivateFileAcl {
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    if ($env:OS -cne 'Windows_NT') {
        throw 'Developer native-host registration is supported only on Windows.'
    }

    $resolvedPath = Resolve-SubtitlerLocalPath -Path $Path -ExpectedType Leaf -MustExist
    $identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    if ($null -eq $identity.User) {
        throw 'Could not determine the current Windows user SID for manifest ACL hardening.'
    }

    # Request and write only the DACL. Get-Acl/Set-Acl can carry an inherited
    # SACL and require SeSecurityPrivilege even for a normal per-user file.
    $fileInfo = [System.IO.FileInfo]::new($resolvedPath)
    $accessSections = [System.Security.AccessControl.AccessControlSections]::Access
    $usesInstanceAclApi = $null -ne $fileInfo.PSObject.Methods['GetAccessControl']
    if ($usesInstanceAclApi) {
        $acl = $fileInfo.GetAccessControl($accessSections)
    }
    else {
        # .NET Core exposes these APIs as extension methods. Calling the static
        # extension class keeps the same DACL-only behavior under pwsh.
        $acl = [System.IO.FileSystemAclExtensions]::GetAccessControl($fileInfo, $accessSections)
    }
    $acl.SetAccessRuleProtection($true, $false)
    $rule = [System.Security.AccessControl.FileSystemAccessRule]::new(
        $identity.User,
        [System.Security.AccessControl.FileSystemRights]::FullControl,
        [System.Security.AccessControl.AccessControlType]::Allow
    )
    $acl.ResetAccessRule($rule)
    if ($usesInstanceAclApi) {
        $fileInfo.SetAccessControl($acl)
    }
    else {
        [System.IO.FileSystemAclExtensions]::SetAccessControl($fileInfo, $acl)
    }
}

function Write-SubtitlerNativeHostManifestAtomically {
    [OutputType([string])]
    param(
        [Parameter(Mandatory)]
        [string]$ManifestPath,

        [Parameter(Mandatory)]
        [string]$ExtensionId,

        [Parameter(Mandatory)]
        [string]$HostExecutable
    )

    $resolvedManifestPath = Resolve-SubtitlerLocalPath -Path $ManifestPath
    $manifestDirectory = Split-Path -Parent $resolvedManifestPath
    $null = Resolve-SubtitlerLocalPath -Path $manifestDirectory -ExpectedType Container -MustExist
    $manifest = New-SubtitlerNativeHostManifest -ExtensionId $ExtensionId -HostExecutable $HostExecutable
    $json = ConvertTo-SubtitlerNativeHostManifestJson -Manifest $manifest
    $temporaryPath = Join-Path $manifestDirectory (".$script:SubtitlerNativeHostName.$([guid]::NewGuid().ToString('N')).tmp")
    $backupPath = Join-Path $manifestDirectory (".$script:SubtitlerNativeHostName.$([guid]::NewGuid().ToString('N')).backup")

    try {
        [System.IO.File]::WriteAllText(
            $temporaryPath,
            $json,
            [System.Text.UTF8Encoding]::new($false)
        )
        Set-SubtitlerPrivateFileAcl -Path $temporaryPath

        $existingManifest = Get-Item -LiteralPath $resolvedManifestPath -Force -ErrorAction SilentlyContinue
        if ($null -ne $existingManifest) {
            if ($existingManifest.PSIsContainer) {
                throw "Manifest destination is a directory: $resolvedManifestPath"
            }

            Assert-SubtitlerNoReparsePointInExistingPath -FullPath $resolvedManifestPath
            try {
                [System.IO.File]::Replace($temporaryPath, $resolvedManifestPath, $backupPath)
            }
            catch {
                throw "Could not atomically replace the existing manifest at $resolvedManifestPath. The previous manifest was left in place. $($_.Exception.Message)"
            }
        }
        else {
            [System.IO.File]::Move($temporaryPath, $resolvedManifestPath)
        }

        # File.Replace may preserve the destination ACL on some filesystems, so
        # harden the published file as well as the temporary source before it
        # is parsed and made reachable through the registry.
        Set-SubtitlerPrivateFileAcl -Path $resolvedManifestPath

        $validationParameters = @{
            ManifestPath          = $resolvedManifestPath
            ExtensionId           = $ExtensionId
            ExpectedHostExecutable = $HostExecutable
            RequireHostExecutable = $true
        }
        $null = Test-SubtitlerNativeHostManifest @validationParameters
        return $resolvedManifestPath
    }
    finally {
        if ([System.IO.File]::Exists($temporaryPath)) {
            [System.IO.File]::Delete($temporaryPath)
        }
        if ([System.IO.File]::Exists($backupPath)) {
            [System.IO.File]::Delete($backupPath)
        }
    }
}

function Get-SubtitlerNativeHostRegistration {
    [OutputType([pscustomobject])]
    param()

    $registryPath = Get-SubtitlerNativeHostRegistryPath
    if (-not (Test-Path -LiteralPath $registryPath)) {
        return $null
    }

    $registryKey = Get-Item -LiteralPath $registryPath -Force
    $subKeys = @($registryKey.GetSubKeyNames())
    $namedValues = @($registryKey.GetValueNames() | Where-Object { $_ -cne '' })
    if ($subKeys.Count -ne 0 -or $namedValues.Count -ne 0) {
        throw 'The existing Subtitler native-host registry key has unexpected subkeys or named values; refusing to modify it.'
    }

    try {
        $defaultValueKind = $registryKey.GetValueKind('')
    }
    catch {
        throw 'The existing Subtitler native-host registry key does not contain a default manifest value; refusing to modify it.'
    }

    if ($defaultValueKind -ne [Microsoft.Win32.RegistryValueKind]::String) {
        throw 'The existing Subtitler native-host registry default must be a REG_SZ string, not an expandable or non-string value; refusing to modify it.'
    }

    $manifestPath = $registryKey.GetValue('', $null)
    if ($manifestPath -isnot [string] -or [string]::IsNullOrWhiteSpace($manifestPath)) {
        throw 'The existing Subtitler native-host registry key does not contain a plain-string default manifest path; refusing to modify it.'
    }

    return [pscustomobject]@{
        RegistryPath = $registryPath
        ManifestPath = $manifestPath
    }
}

function Set-SubtitlerNativeHostRegistration {
    param(
        [Parameter(Mandatory)]
        [string]$ManifestPath
    )

    $resolvedManifestPath = Resolve-SubtitlerLocalPath -Path $ManifestPath -ExpectedType Leaf -MustExist
    $existingRegistration = Get-SubtitlerNativeHostRegistration
    if ($null -ne $existingRegistration) {
        $existingManifestPath = Resolve-SubtitlerLocalPath -Path $existingRegistration.ManifestPath
        if (-not (Test-SubtitlerPathEqual -Left $existingManifestPath -Right $resolvedManifestPath)) {
            throw "A different native-host registration already exists at $($existingRegistration.ManifestPath). Refusing to replace it."
        }
    }

    $registryPath = Get-SubtitlerNativeHostRegistryPath
    if (-not (Test-Path -LiteralPath $registryPath)) {
        New-Item -Path $registryPath -Force | Out-Null
    }

    Set-Item -LiteralPath $registryPath -Value $resolvedManifestPath
}

Export-ModuleMember -Function @(
    'Assert-SubtitlerExtensionId',
    'ConvertTo-SubtitlerNativeHostManifestJson',
    'Get-SubtitlerAllowedOrigin',
    'Get-SubtitlerDeveloperInstallRoot',
    'Get-SubtitlerNativeHostManifestPath',
    'Get-SubtitlerNativeHostName',
    'Get-SubtitlerNativeHostRegistration',
    'Get-SubtitlerNativeHostRegistryPath',
    'New-SubtitlerNativeHostManifest',
    'Resolve-SubtitlerDeveloperInstallDirectory',
    'Resolve-SubtitlerHostExecutable',
    'Resolve-SubtitlerLocalPath',
    'Set-SubtitlerNativeHostRegistration',
    'Test-SubtitlerNativeHostManifest',
    'Test-SubtitlerPathEqual',
    'Write-SubtitlerNativeHostManifestAtomically'
)
