[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$PublicationDirectory,
    [ValidatePattern('^\d+\.\d+\.\d+\.\d+$')][string]$ExpectedVersion
)

# Restore paths after downloading a frozen publication artifact on another
# runner. Never rebuild its ZIPs or change their manifests/content identities.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = [IO.Path]::GetFullPath($PublicationDirectory)
$receiptPath = Join-Path $root 'publication.json'
$receipt = Get-Content -LiteralPath $receiptPath -Raw | ConvertFrom-Json -AsHashtable
if ([string]$receipt.version -notmatch '^\d+\.\d+\.\d+\.\d+$' -or
    ($ExpectedVersion -and $receipt.version -cne $ExpectedVersion)) {
    throw 'Frozen publication version does not match the release.'
}
$seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
foreach ($package in $receipt.packages) {
    $name = [string]$package.packageName
    if (@('cea.cliphost.rust', 'cea.cliphost.rust.arm64', 'cea.clip.quicker.rust', 'cea.clip.quicker.rust.net10') -cnotcontains $name -or
        !$seen.Add($name) -or $package.version -cne $receipt.version) {
        throw 'Unexpected, duplicate or mismatched frozen dependency package.'
    }
    $directory = Join-Path $root $name
    $baseName = if ($name -in @('cea.cliphost.rust', 'cea.cliphost.rust.arm64')) { 'ClipHostRust' } else { 'IntelliTools' }
    $zipName = "$baseName.$($receipt.version).zip"
    $filesSeen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($file in $package.uploads) {
        $fileName = [IO.Path]::GetFileName([string]$file.localPath)
        if (@($zipName, 'version.json') -cnotcontains $fileName -or !$filesSeen.Add($fileName) -or
            $file.remoteKey -cne "$name/$fileName") {
            throw 'Frozen publication file identity changed.'
        }
        $path = Join-Path $directory $fileName
        foreach ($entryPath in @($root, $directory, $path)) {
            $item = Get-Item -LiteralPath $entryPath
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Frozen publication cannot contain linked paths.' }
        }
        if ([string]$file.sha256 -cnotmatch '^[0-9a-f]{64}$' -or
            (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $file.sha256) {
            throw "Frozen publication checksum changed: $name/$fileName"
        }
        if ($fileName -ceq 'version.json') {
            $manifest = Get-Content -LiteralPath $path -Raw | ConvertFrom-Json -AsHashtable
            if ($manifest.packageName -cne $name -or $manifest.version -cne $receipt.version -or
                $manifest.zipFileName -cne $zipName) { throw 'Frozen dependency manifest identity changed.' }
        }
        $file.localPath = $path
    }
    if ($filesSeen.Count -ne 2) { throw 'Frozen publication must contain one ZIP and its version manifest.' }
    $package.payloadDirectory = Join-Path $directory 'payload'
}
$launcherOnly = $receipt.ContainsKey('publicationKind') -and $receipt.publicationKind -ceq 'launchers'
if ($launcherOnly) {
    if ($seen.Count -ne 2 -or !$seen.Contains('cea.clip.quicker.rust') -or !$seen.Contains('cea.clip.quicker.rust.net10')) {
        throw 'A launcher-only publication must include exactly both launcher packages.'
    }
} elseif (!$seen.Contains('cea.cliphost.rust') -and !$seen.Contains('cea.cliphost.rust.arm64')) {
    throw 'Frozen publication must contain the Rust host.'
}
# No files are changed until every frozen payload has passed validation.
$original = Join-Path $root 'source-publication.json'
if (!(Test-Path -LiteralPath $original)) { Copy-Item -LiteralPath $receiptPath -Destination $original }
$temporary = Join-Path $root ('publication-' + [guid]::NewGuid().ToString('N') + '.json')
try {
    $receipt | ConvertTo-Json -Depth 20 | Set-Content -LiteralPath $temporary -Encoding utf8NoBOM
    [IO.File]::Move($temporary, $receiptPath, $true)
} finally { if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary } }
Write-Output $receiptPath
