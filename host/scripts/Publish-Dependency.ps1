[CmdletBinding(DefaultParameterSetName = 'Prepare')]
param(
    [Parameter(Mandatory, ParameterSetName = 'Prepare')][string]$PackageDirectory,
    [Parameter(Mandatory, ParameterSetName = 'Prepare')]
    [Parameter(Mandatory, ParameterSetName = 'PrepareLaunchers')]
    [ValidatePattern('^\d+\.\d+\.\d+\.\d+$')][string]$Version,
    [Parameter(ParameterSetName = 'Prepare')]
    [Parameter(ParameterSetName = 'PrepareLaunchers')]
    [ValidatePattern('^\d+\.\d+\.\d+\.\d+$')][string]$LauncherVersion,
    [Parameter(ParameterSetName = 'Prepare')]
    [Parameter(ParameterSetName = 'PrepareLaunchers')]
    [ValidatePattern('^\d+\.\d+\.\d+\.\d+$')][string]$RequiredLauncherVersion,
    [Parameter(ParameterSetName = 'Prepare')]
    [Parameter(Mandatory, ParameterSetName = 'PrepareLaunchers')][string]$LauncherV1Directory,
    [Parameter(ParameterSetName = 'Prepare')]
    [Parameter(Mandatory, ParameterSetName = 'PrepareLaunchers')][string]$LauncherV2Directory,
    [Parameter(Mandatory, ParameterSetName = 'PrepareLaunchers')][switch]$LauncherOnly,
    [Parameter(ParameterSetName = 'Prepare')][ValidateSet('x64', 'arm64')][string]$Architecture = 'x64',
    [Parameter(ParameterSetName = 'Prepare')]
    [ValidateSet('Gpui', 'Tauri')][string]$Renderer = 'Gpui',
    [Parameter(Mandatory, ParameterSetName = 'UploadPrepared')]
    [Parameter(ParameterSetName = 'Prepare')]
    [Parameter(ParameterSetName = 'PrepareLaunchers')][string]$OutputDirectory,
    [Parameter(ParameterSetName = 'Prepare')]
    [Parameter(ParameterSetName = 'PrepareLaunchers')][switch]$Upload,
    [Parameter(Mandatory, ParameterSetName = 'UploadPrepared')][switch]$UploadPrepared,
    [Parameter(ParameterSetName = 'Prepare')]
    [Parameter(ParameterSetName = 'PrepareLaunchers')]
    [Parameter(ParameterSetName = 'UploadPrepared')][switch]$OssOnly,
    [Parameter(ParameterSetName = 'Prepare')]
    [Parameter(ParameterSetName = 'PrepareLaunchers')]
    [Parameter(ParameterSetName = 'UploadPrepared')][switch]$QuickerOnly,
    [Parameter(Mandatory, ParameterSetName = 'Credentials')][switch]$CheckUploadCredentials,
    [string]$EnvFile
)

# Prepare independent Rust dependencies; explicit uploads include the official library.
# Neither mode publishes actions/subprograms or touches C# version.json/packages.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (!$IsWindows) { throw 'Run with PowerShell 7 on Windows.' }
# Load only this uploader's settings; do not evaluate dotenv syntax or import account logins.
# Existing process settings take precedence, as with a preconfigured upload shell.
if ($EnvFile) {
    $uploadSettings = @('BITIFUL_ACCESS_KEY', 'BITIFUL_SECRET_KEY', 'BITIFUL_BUCKET_NAME',
        'BITIFUL_ENDPOINT_URL', 'BITIFUL_REGION')
    foreach ($line in [IO.File]::ReadLines([IO.Path]::GetFullPath($EnvFile))) {
        if ($line -notmatch '^\s*(?:export\s+)?([A-Z_]+)\s*=\s*(.*?)\s*$') { continue }
        $name = $Matches[1]
        if ($uploadSettings -cnotcontains $name -or
            ![string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($name))) { continue }
        $value = $Matches[2]
        if ($value.Length -ge 2 -and
            (($value[0] -eq '"' -and $value[-1] -eq '"') -or
             ($value[0] -eq "'" -and $value[-1] -eq "'"))) {
            $value = $value.Substring(1, $value.Length - 2)
        }
        [Environment]::SetEnvironmentVariable($name, $value)
    }
}
if ($CheckUploadCredentials) {
    [ordered]@{
        accessKeyPresent = ![string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable('BITIFUL_ACCESS_KEY'))
        secretKeyPresent = ![string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable('BITIFUL_SECRET_KEY'))
    } | ConvertTo-Json
    return
}
$rustRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$clipRoot = [IO.Path]::GetFullPath((Join-Path $rustRoot '..'))
$ClipRepoRoot = $clipRoot
if (!$OutputDirectory) {
    $OutputDirectory = Join-Path $ClipRepoRoot ('.tmp/cliphost-rust-dependencies-' + [guid]::NewGuid().ToString('N'))
}
if (($OssOnly -or $QuickerOnly) -and !$Upload -and !$UploadPrepared) {
    throw '-OssOnly and -QuickerOnly require -Upload or -UploadPrepared.'
}
if ($OssOnly -and $QuickerOnly) { throw '-OssOnly and -QuickerOnly cannot be combined.' }
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$receiptPath = Join-Path $OutputDirectory 'publication.json'

function Write-PackageJson($Value, [string]$Path) {
    $Value | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $Path -Encoding utf8NoBOM
}

function Get-PackageHash([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function New-PackageManifest([string]$Name, [string]$ZipName) {
    $parts = $Version.Split('.')
    $manifestLauncher = if ($Name -in @('cea.cliphost.rust', 'cea.cliphost.rust.arm64')) { $RequiredLauncherVersion } else { $LauncherVersion }
    $manifest = [ordered]@{
        release = ($parts[0..2] -join '.'); build = [int]$parts[3]
        packageName = $Name; version = $Version; launcherVersion = $manifestLauncher
        CeaQuicker = $manifestLauncher
        zipFileName = $ZipName
    }
    # A launcher-only release does not imply a backend with the same version exists.
    if (!$LauncherOnly) { $manifest.ClipHostRust = $Version }
    $manifest
}

function New-DependencyArchive([string]$Name, [string]$Payload, [string]$BaseName) {
    $folder = Split-Path -Parent $Payload
    $zipName = "$BaseName.$Version.zip"
    $zipPath = Join-Path $folder $zipName
    # No wrapper folder: dependency extraction finds the executable/DLLs at its root.
    . (Join-Path $PSScriptRoot 'New-ZipArchive.ps1')
    New-ClipHostZipArchive -SourceDirectory $Payload -ArchivePath $zipPath
    $manifest = Join-Path $folder 'version.json'
    Write-PackageJson (New-PackageManifest $Name $zipName) $manifest
    [ordered]@{
        packageName = $Name; version = $Version; payloadDirectory = $Payload
        uploads = @(
            [ordered]@{ localPath=$zipPath; remoteKey="$Name/$zipName"; sha256=(Get-PackageHash $zipPath) },
            [ordered]@{ localPath=$manifest; remoteKey="$Name/version.json"; sha256=(Get-PackageHash $manifest) }
        )
    }
}

function Prepare-RustDependency {
    $sourceRoot = [IO.Path]::GetFullPath($PackageDirectory)
    $sourceInfo = Join-Path $sourceRoot 'build-info.json'
    $info = Get-Content -LiteralPath $sourceInfo -Raw | ConvertFrom-Json -AsHashtable
    if ($info.product -cne 'ClipHost Rust' -or $info.profile -cne 'release' -or
        $info.embeddedFrontend -ne ($Renderer -eq 'Tauri') -or $info.executable -cne 'cliphost-app.exe' -or
        ($Renderer -eq 'Gpui' -and $info.renderer -cne 'gpui') -or
        ($info.ContainsKey('renderer') -and $info.renderer -cne $Renderer.ToLowerInvariant())) {
        throw 'PackageDirectory must be a validated standalone Release package.'
    }
    . (Join-Path $PSScriptRoot 'Read-ExecutableBuildInfo.ps1')
    . (Join-Path $PSScriptRoot 'Assert-StandaloneBuildInfo.ps1')
    . (Join-Path $PSScriptRoot 'Read-NativeSymbols.ps1')
    $exe = Join-Path $sourceRoot 'cliphost-app.exe'
    $executableBuild = Read-ClipHostExecutableBuildInfo -Path $exe
    Assert-ClipHostStandaloneBuildInfo -BuildInfo $executableBuild -Renderer $Renderer
    $expectedTarget = if ($Architecture -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
    $packageName = if ($Architecture -eq 'arm64') { 'cea.cliphost.rust.arm64' } else { 'cea.cliphost.rust' }
    if (($info.ContainsKey('architecture') -and $info.architecture -cne $Architecture) -or
        ($info.ContainsKey('target') -and $info.target -cne $expectedTarget) -or
        ($executableBuild.Contains('target') -and $executableBuild.target -cne $expectedTarget) -or
        ($Architecture -eq 'arm64' -and (!$info.ContainsKey('architecture') -or !$executableBuild.Contains('target')))) {
        throw "The Quicker host dependency requires $Architecture. Select the matching -Architecture and a validated executable."
    }
    $releaseVersion = ($Version.Split('.')[0..2] -join '.')
    $executableVersion = [string]$executableBuild['version']
    if ($executableVersion -cne $releaseVersion) {
        throw "Executable version '$executableVersion' does not match package release '$releaseVersion'."
    }
    if ((Get-PackageHash $exe) -cne $info.sha256) { throw 'Standalone executable checksum changed.' }
    $payload = Join-Path $OutputDirectory "$packageName/payload"
    New-Item -ItemType Directory -Path $payload -Force | Out-Null
    $copied = @(foreach ($file in $info.files) {
        $name = [string]$file.name
        if ([IO.Path]::GetFileName($name) -cne $name -or $name.Contains('/') -or
            ($name -cne 'cliphost-app.exe' -and [IO.Path]::GetExtension($name) -ine '.dll' -and
             [IO.Path]::GetExtension($name) -ine '.pdb' -and
             $name -cnotin @('Start-ClipHostDiagnostics.ps1', 'Start-ClipHostDiagnostics.cmd'))) {
            throw "Unexpected standalone payload file: $name"
        }
        $inputPath = Join-Path $sourceRoot $name
        $stream = [IO.File]::OpenRead($inputPath)
        try {
            $hash = (Get-FileHash -InputStream $stream -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($hash -cne $file.sha256 -or $stream.Length -ne $file.sizeBytes) {
                throw "Standalone payload checksum changed: $name"
            }
            if ([IO.Path]::GetExtension($name) -ieq '.pdb') {
                $codeView = Read-ClipHostCodeViewIdentity -Path $exe
                Assert-ClipHostNativePdb -CodeView $codeView -Stream $stream -Name $name
                continue
            }
            # Published launchers accept only the existing native dependency
            # format. Verify portable symbols/diagnostics too, but distribute them
            # separately so recovery does not require a launcher upgrade.
            if ($name -cin @('Start-ClipHostDiagnostics.ps1', 'Start-ClipHostDiagnostics.cmd')) { continue }
            $stream.Position = 0
            $target = [IO.File]::Open((Join-Path $payload $name), [IO.FileMode]::CreateNew)
            try { $stream.CopyTo($target) } finally { $target.Dispose() }
            [ordered]@{ name=$name; sha256=$hash; sizeBytes=$stream.Length }
        } finally { $stream.Dispose() }
    })
    if (@($copied | Where-Object name -ceq 'cliphost-app.exe').Count -ne 1 -or
        @($copied | Where-Object name -ceq 'vcruntime140.dll').Count -ne 1) {
        throw 'The standalone manifest must contain the executable and redistributable VC runtime.'
    }
    $info['packageName'] = $packageName
    $info['version'] = $Version
    $info['renderer'] = $Renderer.ToLowerInvariant()
    # Existing launchers read this field as the compatibility floor, not the build version.
    $info['launcherVersion'] = $RequiredLauncherVersion
    $info['files'] = $copied
    $info['executableBuild'] = $executableBuild
    Write-PackageJson $info (Join-Path $payload 'build-info.json')
    New-DependencyArchive $packageName $payload 'ClipHostRust'
}

function Prepare-LauncherDependency([string]$InputDirectory, [string]$Name) {
    $inputRoot = [IO.Path]::GetFullPath($InputDirectory)
    foreach ($assembly in @('IntelliTools', 'IntelliTools.Quicker')) {
        $assemblyPath = Join-Path $inputRoot "$assembly.$LauncherVersion.dll"
        $identity = [Reflection.AssemblyName]::GetAssemblyName($assemblyPath)
        if ($identity.Name -cne "$assembly.$LauncherVersion" -or $identity.Version.ToString() -cne $LauncherVersion) {
            throw "Launcher assembly identity does not match $LauncherVersion : $assemblyPath"
        }
    }
    $payload = Join-Path $OutputDirectory "$Name/payload"
    New-Item -ItemType Directory -Path $payload -Force | Out-Null
    # These are reviewed qkbuild payload roots, including frontend/native subdirectories.
    foreach ($item in Get-ChildItem -LiteralPath $inputRoot -Force) {
        Copy-Item -LiteralPath $item.FullName -Destination $payload -Recurse
    }
    New-DependencyArchive $Name $payload 'IntelliTools'
}

if (!$UploadPrepared) {
    $compatibility = Get-Content -LiteralPath (Join-Path $rustRoot 'launcher-compatibility.json') -Raw | ConvertFrom-Json -AsHashtable
    $baselineKey = if (!$LauncherOnly -and $Architecture -eq 'arm64') { 'arm64RequiredLauncherVersion' } else { 'requiredLauncherVersion' }
    $baseline = [string]$compatibility[$baselineKey]
    $baselineVersion = $null
    if ($baseline -notmatch '^\d+\.\d+\.\d+\.\d+$' -or ![version]::TryParse($baseline, [ref]$baselineVersion)) {
        throw 'The launcher compatibility baseline must be a valid four-part version.'
    }
    if (!$RequiredLauncherVersion) { $RequiredLauncherVersion = $baseline }
    if (!$LauncherVersion) {
        $LauncherVersion = $Version
        # A separately published launcher may be newer than an existing host.
        # Only supplied DLL payloads claim an actual assembly identity below.
        if (!$LauncherV1Directory -and !$LauncherV2Directory -and
            $baselineVersion -gt [version]$LauncherVersion) {
            $LauncherVersion = $baseline
        }
    }
    if ([version]$RequiredLauncherVersion -lt [version]$baseline) {
        throw 'RequiredLauncherVersion cannot be lower than the reviewed compatibility baseline.'
    }
    if ([version]$RequiredLauncherVersion -gt [version]$LauncherVersion) {
        throw 'The required launcher version cannot exceed the actual launcher version.'
    }
    if ([version]$RequiredLauncherVersion -gt [version]$baseline -and
        (!$LauncherV1Directory -or !$LauncherV2Directory)) {
        throw 'Raising the required launcher version requires both V1 and V2 launcher payloads.'
    }
    if (($LauncherV1Directory -or $LauncherV2Directory) -and $LauncherVersion -cne $Version) {
        throw 'When publishing launcher payloads, their assembly version must match the dependency archive version.'
    }
    if (Test-Path -LiteralPath $OutputDirectory) {
        throw 'Choose a new OutputDirectory; prepared publications are never overwritten.'
    }
    New-Item -ItemType Directory -Path $OutputDirectory | Out-Null
    $packages = @(
        if ($LauncherV1Directory) { Prepare-LauncherDependency $LauncherV1Directory 'cea.clip.quicker.rust' }
        if ($LauncherV2Directory) { Prepare-LauncherDependency $LauncherV2Directory 'cea.clip.quicker.rust.net10' }
        if (!$LauncherOnly) { Prepare-RustDependency }
    )
    Write-PackageJson ([ordered]@{ version=$Version; launcherVersion=$LauncherVersion
        requiredLauncherVersion=$RequiredLauncherVersion
        publicationKind=$(if ($LauncherOnly) { 'launchers' } else { 'host' }); packages=$packages }) $receiptPath
}

if ($Upload -or $UploadPrepared) {
    if ($QuickerOnly) {
        # The first DLL is acquired by Quicker's built-in dependency download.
        # Official-only publication must not inspect or require Bitiful secrets.
        . (Join-Path $PSScriptRoot 'Publish-QuickerDependency.ps1')
        Publish-ClipHostQuickerDependency -PublicationDirectory $OutputDirectory
        Write-Output $receiptPath
        return
    }
    foreach ($credential in @('BITIFUL_ACCESS_KEY', 'BITIFUL_SECRET_KEY')) {
        if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($credential))) {
            throw "$credential is not configured; use -CheckUploadCredentials for a value-free check."
        }
    }
    # Only non-secret defaults are reused. Credentials remain in the process environment.
    $defaults = Get-Content -LiteralPath (Join-Path $clipRoot 'scripts/bitiful-cliphost.defaults.json') -Raw | ConvertFrom-Json
    foreach ($setting in @(@('BITIFUL_BUCKET_NAME', 'bucketName'), @('BITIFUL_ENDPOINT_URL', 'endpointUrl'), @('BITIFUL_REGION', 'region'))) {
        if (![Environment]::GetEnvironmentVariable($setting[0])) {
            [Environment]::SetEnvironmentVariable($setting[0], [string]$defaults.($setting[1]))
        }
    }
    if ([Environment]::GetEnvironmentVariable('BITIFUL_BUCKET_NAME').Trim() -cne $defaults.bucketName -or
        [Environment]::GetEnvironmentVariable('BITIFUL_ENDPOINT_URL').Trim().TrimEnd('/') -cne $defaults.endpointUrl.TrimEnd('/')) {
        throw 'This dependency publisher requires the configured quicker-pkgs Bitiful destination.'
    }
    $receipt = Get-Content -LiteralPath $receiptPath -Raw | ConvertFrom-Json
    $requiredLauncher = if ($receipt.PSObject.Properties.Name -ccontains 'requiredLauncherVersion') {
        [string]$receipt.requiredLauncherVersion
    } else { [string]$receipt.launcherVersion }
    if ([string]$receipt.launcherVersion -notmatch '^\d+\.\d+\.\d+\.\d+$' -or
        $requiredLauncher -notmatch '^\d+\.\d+\.\d+\.\d+$' -or
        [version]$requiredLauncher -gt [version]$receipt.launcherVersion) {
        throw 'Invalid prepared launcher compatibility version.'
    }
    . (Join-Path $PSScriptRoot 'BitifulPublication.ps1')
    # Inspect every frozen object and remote version before the first mutation.
    # Hold local files open against writes until all upload stages finish.
    $plan = [Collections.Generic.List[object]]::new()
    $lockedFiles = [Collections.Generic.List[IO.FileStream]]::new()
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $allowed = @('cea.cliphost.rust', 'cea.cliphost.rust.arm64', 'cea.clip.quicker.rust', 'cea.clip.quicker.rust.net10')
    $remoteClient = [Net.Http.HttpClient]::new()
    try {
        # Entry DLLs are acquired from Quicker's official dependency library.
        # A host mirror must not require another copy of those DLLs on Bitiful.
        # The official publisher below verifies the compatible launcher pair.
        foreach ($package in ($receipt.packages | Sort-Object {
            switch ($_.packageName) {
                'cea.clip.quicker.rust' { 0 }
                'cea.clip.quicker.rust.net10' { 1 }
                default { 2 }
            }
        })) {
            if ($allowed -cnotcontains $package.packageName -or !$seen.Add($package.packageName) -or
                $package.version -cne $receipt.version) { throw 'Unexpected, duplicate, or mismatched upload package.' }
            $files = @{}
            foreach ($file in $package.uploads) {
                $path = [IO.Path]::GetFullPath($file.localPath)
                if (!$path.StartsWith($OutputDirectory.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar,
                        [StringComparison]::OrdinalIgnoreCase) -or
                    $file.remoteKey -cne ($package.packageName + '/' + [IO.Path]::GetFileName($path))) {
                    throw 'Prepared publication paths or checksums changed; upload cancelled.'
                }
                $stream = [IO.File]::Open($path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
                $lockedFiles.Add($stream)
                if ((Get-FileHash -InputStream $stream -Algorithm SHA256).Hash.ToLowerInvariant() -cne $file.sha256) {
                    throw 'Prepared publication paths or checksums changed; upload cancelled.'
                }
                $isManifest = [IO.Path]::GetFileName($path) -ceq 'version.json'
                if (!$isManifest) {
                    $baseName = if ($package.packageName -in @('cea.cliphost.rust', 'cea.cliphost.rust.arm64')) { 'ClipHostRust' } else { 'IntelliTools' }
                    if ([IO.Path]::GetFileName($path) -cne "$baseName.$($receipt.version).zip") {
                        throw 'Unexpected versioned archive; upload cancelled.'
                    }
                }
                $kind = if ($isManifest) { 'manifest' } else { 'archive' }
                if ($files.ContainsKey($kind)) { throw 'Expected one archive and one version.json per dependency.' }
                $state = Get-ClipHostBitifulObjectState -Client $remoteClient -RemoteKey $file.remoteKey `
                    -LocalPath $path -ExpectedHash $file.sha256 -VersionManifest:$isManifest
                if ($isManifest -and $state.version -cne $receipt.version) {
                    throw 'Prepared version.json does not match the publication version.'
                }
                $files[$kind] = $state
            }
            if ($files.Count -ne 2) { throw 'Expected one archive and one version.json per dependency.' }
            $plan.Add($files)
        }
        $launcherOnlyReceipt = $receipt.PSObject.Properties.Name -ccontains 'publicationKind' -and $receipt.publicationKind -ceq 'launchers'
        if ($launcherOnlyReceipt) {
            if ($seen.Count -ne 2 -or !$seen.Contains('cea.clip.quicker.rust') -or !$seen.Contains('cea.clip.quicker.rust.net10')) {
                throw 'A launcher-only publication must include exactly both launcher packages.'
            }
        } elseif (!$seen.Contains('cea.cliphost.rust') -and !$seen.Contains('cea.cliphost.rust.arm64')) {
            throw 'Prepared publication must include the Rust host.'
        }
        $uploader = Join-Path $clipRoot 'tools/BitifulUpload/BitifulUpload.csproj'
        $verified = [Collections.Generic.List[object]]::new()
        foreach ($packagePlan in $plan) {
            # Never advance a pointer until its ZIP was uploaded and downloaded successfully.
            # Each invocation contains one object: a failure cannot publish the next object.
            foreach ($kind in @('archive', 'manifest')) {
                $file = $packagePlan[$kind]
                if ($file.status -ceq 'same') {
                    Write-Host "Already present with identical checksum: $($file.remoteKey)"
                } else {
                    & dotnet run --project $uploader -c Release --artifacts-path (Join-Path $OutputDirectory 'uploader-build') -- upload `
                        --file "$($file.localPath):$($file.remoteKey)"
                    if ($LASTEXITCODE -ne 0) { throw "Dependency upload failed: $LASTEXITCODE. Check remote state before retrying." }
                    Assert-ClipHostBitifulObjectHash -Client $remoteClient -RemoteKey $file.remoteKey `
                        -LocalPath $file.localPath -ExpectedHash $file.sha256
                }
                $verified.Add([ordered]@{ remoteKey=$file.remoteKey; sha256=$file.sha256
                    sizeBytes=$file.sizeBytes; status=$(if ($file.status -ceq 'same') { 'already-present' } else { 'uploaded' }) })
                Write-PackageJson ([ordered]@{ version=$receipt.version; files=@($verified.ToArray()) }) `
                    (Join-Path $OutputDirectory 'bitiful-upload.json')
            }
        }
    } finally {
        $remoteClient.Dispose()
        foreach ($stream in $lockedFiles) { $stream.Dispose() }
    }
    if (!$OssOnly) {
        . (Join-Path $PSScriptRoot 'Publish-QuickerDependency.ps1')
        Publish-ClipHostQuickerDependency -PublicationDirectory $OutputDirectory
    } else {
        Write-Host 'Bitiful backup source verified. Official dependency and launcher availability were not checked (-OssOnly).'
    }
}

Write-Output $receiptPath
