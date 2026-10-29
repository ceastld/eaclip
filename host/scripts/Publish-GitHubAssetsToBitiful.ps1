[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$AssetDirectory,
    [Parameter(Mandatory)][ValidatePattern('^\d+\.\d+\.\d+$')][string]$Version,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [ValidateSet('x64', 'arm64')][string[]]$Architectures = @('x64', 'arm64')
)

# Mirror the exact GitHub attachments, including ARM64, under a separate prefix.
# This never changes Quicker dependency identities or any actions.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'BitifulPublication.ps1')
$clipRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$AssetDirectory = [IO.Path]::GetFullPath($AssetDirectory)
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $OutputDirectory) { throw "Output already exists: $OutputDirectory" }
if (!$Architectures.Count -or @($Architectures | Select-Object -Unique).Count -ne $Architectures.Count) {
    throw 'Select at least one architecture without duplicates.'
}
$expected = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
$files = [Collections.Generic.List[object]]::new()
$heldFiles = [Collections.Generic.List[IO.Stream]]::new()
$client = [Net.Http.HttpClient]::new()
try {
    foreach ($arch in $Architectures) {
        $stem = "eaclip-$Version-windows-$arch"
        $names = @("$stem.zip", "$stem.build-info.json", "$stem.SHA256SUMS.txt")
        foreach ($name in $names) { [void]$expected.Add($name) }
        $info = Get-Content -LiteralPath (Join-Path $AssetDirectory $names[1]) -Raw | ConvertFrom-Json -AsHashtable
        $target = if ($arch -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
        if ($info.product -cne 'ClipHost Rust' -or $info.version -cne "$Version.0" -or
            $info.architecture -cne $arch -or $info.target -cne $target -or
            $info.renderer -cne 'gpui' -or $info.profile -cne 'release' -or $info.embeddedFrontend -ne $false) {
            throw "Release asset metadata does not match $arch $Version."
        }
        $checksums = @{}
        foreach ($line in Get-Content -LiteralPath (Join-Path $AssetDirectory $names[2])) {
            if ($line -cnotmatch '^([0-9a-f]{64})  (.+)$' -or
                @($names[0], $names[1]) -cnotcontains $Matches[2] -or $checksums.ContainsKey($Matches[2])) {
                throw 'Invalid or duplicate release attachment checksum.'
            }
            $checksums[$Matches[2]] = $Matches[1]
        }
        if ($checksums.Count -ne 2) { throw 'Incomplete release attachment checksums.' }
        foreach ($name in $names) {
            $path = Join-Path $AssetDirectory $name
            $item = Get-Item -LiteralPath $path
            if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                throw 'Release attachments must be ordinary files.'
            }
            $stream = [IO.File]::OpenRead($path)
            $heldFiles.Add($stream)
            $hash = (Get-FileHash -InputStream $stream -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($checksums.ContainsKey($name) -and $checksums[$name] -cne $hash) { throw "Release attachment checksum changed: $name" }
            $files.Add([pscustomobject]@{ localPath=$path; remoteKey="eaclip/rust-v$Version/$name"; sha256=$hash })
        }
    }
    foreach ($item in Get-ChildItem -LiteralPath $AssetDirectory -Force) {
        if (!$expected.Contains($item.Name) -or $item.PSIsContainer) { throw 'Expected exactly the selected architecture-specific release attachments.' }
    }
    $plans = @(foreach ($file in $files) {
        Get-ClipHostBitifulObjectState -Client $client -RemoteKey $file.remoteKey -LocalPath $file.localPath -ExpectedHash $file.sha256
    })
    $defaults = Get-Content -LiteralPath (Join-Path $clipRoot 'scripts/bitiful-cliphost.defaults.json') -Raw | ConvertFrom-Json
    foreach ($setting in @(@('BITIFUL_BUCKET_NAME', 'bucketName'), @('BITIFUL_ENDPOINT_URL', 'endpointUrl'), @('BITIFUL_REGION', 'region'))) {
        if (![Environment]::GetEnvironmentVariable($setting[0])) {
            [Environment]::SetEnvironmentVariable($setting[0], [string]$defaults.($setting[1]))
        }
    }
    if ($env:BITIFUL_BUCKET_NAME.Trim() -cne $defaults.bucketName -or
        $env:BITIFUL_ENDPOINT_URL.Trim().TrimEnd('/') -cne $defaults.endpointUrl.TrimEnd('/')) {
        throw 'Release mirroring requires the configured quicker-pkgs Bitiful destination.'
    }
    if (@($plans | Where-Object status -cne 'same').Count -gt 0) {
        foreach ($credential in @('BITIFUL_ACCESS_KEY', 'BITIFUL_SECRET_KEY')) {
            if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($credential))) {
                throw "$credential is not configured."
            }
        }
    }
    New-Item -ItemType Directory -Path $OutputDirectory | Out-Null
    $receiptPath = Join-Path $OutputDirectory 'bitiful-assets.json'
    $results = [Collections.Generic.List[object]]::new()
    foreach ($plan in $plans) {
        $status = 'already-present'
        if ($plan.status -cne 'same') {
            & dotnet run --project (Join-Path $clipRoot 'tools/BitifulUpload/BitifulUpload.csproj') -c Release `
                --artifacts-path (Join-Path $OutputDirectory 'uploader-build') -- upload --file "$($plan.localPath):$($plan.remoteKey)"
            if ($LASTEXITCODE -ne 0) { throw 'Bitiful release upload failed. Check the remote object before retrying.' }
            Assert-ClipHostBitifulObjectHash -Client $client -RemoteKey $plan.remoteKey -LocalPath $plan.localPath -ExpectedHash $plan.sha256
            $status = 'uploaded'
        }
        $results.Add([ordered]@{ remoteKey=$plan.remoteKey; status=$status; sha256=$plan.sha256; sizeBytes=$plan.sizeBytes })
        [ordered]@{ version=$Version; files=$results.ToArray() } | ConvertTo-Json -Depth 8 |
            Set-Content -LiteralPath $receiptPath -Encoding utf8NoBOM
        Write-Host "Bitiful release asset verified: $($plan.remoteKey) ($status)"
    }
    Write-Output $receiptPath
} finally {
    $client.Dispose()
    foreach ($stream in $heldFiles) { $stream.Dispose() }
}
