[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$PackageDirectory,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [Parameter(Mandatory)][ValidateSet('x64', 'arm64')][string]$ExpectedArchitecture
)

# Prepare public release assets from a validated standalone package. No account,
# network, installation or running host is involved.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (!$IsWindows) { throw 'Validate executable resources with PowerShell 7 on Windows.' }
. (Join-Path $PSScriptRoot 'Read-ExecutableBuildInfo.ps1')
. (Join-Path $PSScriptRoot 'Assert-StandaloneBuildInfo.ps1')
. (Join-Path $PSScriptRoot 'Read-PortableExecutable.ps1')
. (Join-Path $PSScriptRoot 'New-ZipArchive.ps1')

$PackageDirectory = [IO.Path]::GetFullPath($PackageDirectory)
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $OutputDirectory) { throw "Output already exists: $OutputDirectory" }
$infoPath = Join-Path $PackageDirectory 'build-info.json'
$info = Get-Content -LiteralPath $infoPath -Raw | ConvertFrom-Json -AsHashtable
$expectedTarget = if ($ExpectedArchitecture -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
if ($info.product -cne 'ClipHost Rust' -or $info.renderer -cne 'gpui' -or
    $info.profile -cne 'release' -or $info.embeddedFrontend -ne $false -or
    $info.executable -cne 'cliphost-app.exe' -or $info.architecture -cne $ExpectedArchitecture -or
    $info.target -cne $expectedTarget) {
    throw 'Expected a GPUI standalone Release package for the requested architecture.'
}
$exe = Join-Path $PackageDirectory 'cliphost-app.exe'
$pe = Read-ClipHostPortableExecutable -Path $exe
$executableBuild = Read-ClipHostExecutableBuildInfo -Path $exe
Assert-ClipHostStandaloneBuildInfo -BuildInfo $executableBuild -Renderer Gpui
if ($pe.architecture -cne $ExpectedArchitecture -or $pe.target -cne $expectedTarget -or
    $executableBuild.target -cne $expectedTarget -or
    $info.version -cne ($executableBuild.version + '.0')) {
    throw 'Package architecture, target or version does not match the actual executable.'
}
if ($executableBuild.version -cnotmatch '^\d+\.\d+\.\d+$') { throw 'Invalid executable version.' }
$version = $executableBuild.version
$allowedNames = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($name in @('build-info.json', 'SHA256SUMS.txt', 'README.txt')) { [void]$allowedNames.Add($name) }
foreach ($file in $info.files) {
    $name = [string]$file.name
    if (!$name -or $name -match '[\\/:]' -or $name -in @('.', '..') -or !$allowedNames.Add($name)) {
        throw "Invalid or duplicate package payload name: $name"
    }
    if ([IO.Path]::GetExtension($name) -ieq '.pdb') {
        throw 'Public user packages must not contain PDB files; retain native symbols separately.'
    }
    $path = Join-Path $PackageDirectory $name
    $item = Get-Item -LiteralPath $path
    if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
        $item.Length -ne $file.sizeBytes -or
        (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $file.sha256) {
        throw "Package payload size or checksum mismatch: $name"
    }
    if ($item.Extension -in @('.exe', '.dll')) {
        $payloadPe = Read-ClipHostPortableExecutable -Path $path
        if ($payloadPe.architecture -cne $ExpectedArchitecture) { throw "Package payload architecture mismatch: $name" }
    }
}
if (!($info.files | Where-Object name -CEQ 'cliphost-app.exe') -or
    !($info.files | Where-Object name -CEQ 'vcruntime140.dll') -or
    $info.sha256 -cne (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()) {
    throw 'Package manifest is missing the executable/runtime or has a different executable checksum.'
}
foreach ($item in Get-ChildItem -LiteralPath $PackageDirectory -Force) {
    if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or !$allowedNames.Contains($item.Name)) {
        throw "Unexpected package entry: $($item.Name)"
    }
}

New-Item -ItemType Directory -Path $OutputDirectory | Out-Null
$stem = "eaclip-$version-windows-$ExpectedArchitecture"
$archive = Join-Path $OutputDirectory "$stem.zip"
New-ClipHostZipArchive -SourceDirectory $PackageDirectory -ArchivePath $archive -RootDirectory 'ClipHost-Rust'
$manifest = Join-Path $OutputDirectory "$stem.build-info.json"
Copy-Item -LiteralPath $infoPath -Destination $manifest
@($archive, $manifest) | ForEach-Object {
    '{0}  {1}' -f (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash.ToLowerInvariant(), [IO.Path]::GetFileName($_)
} | Set-Content -LiteralPath (Join-Path $OutputDirectory "$stem.SHA256SUMS.txt") -Encoding utf8NoBOM
[ordered]@{ version=$version; architecture=$ExpectedArchitecture; target=$expectedTarget; archive=$archive; manifest=$manifest } | ConvertTo-Json
