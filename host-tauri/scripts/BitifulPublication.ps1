# Shared by Rust dependency and portable-asset publishers. Dot-sourcing never uploads.
# All reads use the public destination; credentials belong only to the uploader process.
function Read-ClipHostBitifulObject {
    param(
        [Parameter(Mandatory)][Net.Http.HttpClient]$Client,
        [Parameter(Mandatory)][string]$RemoteKey,
        [switch]$VersionManifest
    )
    if ($RemoteKey.Contains('\') -or $RemoteKey.StartsWith('/') -or
        @($RemoteKey.Split('/') | Where-Object { $_ -in @('', '.', '..') }).Count -gt 0) {
        throw 'Invalid Bitiful object key.'
    }
    $escapedKey = ($RemoteKey.Split('/') | ForEach-Object { [Uri]::EscapeDataString($_) }) -join '/'
    $uri = [Uri]::new("https://quicker-pkgs.s3.bitiful.net/$escapedKey")
    $timeout = [Threading.CancellationTokenSource]::new([TimeSpan]::FromSeconds(60))
    $request = [Net.Http.HttpRequestMessage]::new([Net.Http.HttpMethod]::Get, $uri)
    $request.Headers.CacheControl = [Net.Http.Headers.CacheControlHeaderValue]::new()
    $request.Headers.CacheControl.NoCache = $true
    $response = $null
    $stream = $null
    $hasher = [Security.Cryptography.IncrementalHash]::CreateHash([Security.Cryptography.HashAlgorithmName]::SHA256)
    $manifest = if ($VersionManifest) { [IO.MemoryStream]::new() } else { $null }
    try {
        $response = $Client.SendAsync($request, [Net.Http.HttpCompletionOption]::ResponseHeadersRead,
            $timeout.Token).GetAwaiter().GetResult()
        if ($response.StatusCode -eq [Net.HttpStatusCode]::NotFound) { return @{ missing=$true } }
        if (!$response.IsSuccessStatusCode) {
            throw "Cannot verify Bitiful object $RemoteKey (HTTP $([int]$response.StatusCode)); publication cancelled."
        }
        $stream = $response.Content.ReadAsStreamAsync($timeout.Token).GetAwaiter().GetResult()
        $buffer = [byte[]]::new(65536)
        $length = [long]0
        while (($count = $stream.ReadAsync($buffer, 0, $buffer.Length, $timeout.Token).GetAwaiter().GetResult()) -gt 0) {
            $hasher.AppendData($buffer, 0, $count)
            $length += $count
            if ($VersionManifest) {
                if ($length -gt 1048576) { throw "Bitiful version manifest is too large: $RemoteKey" }
                $manifest.Write($buffer, 0, $count)
            }
        }
        if ($null -ne $response.Content.Headers.ContentLength -and
            $response.Content.Headers.ContentLength -ne $length) {
            throw "Incomplete Bitiful download: $RemoteKey"
        }
        $result = @{ missing=$false; sizeBytes=$length
            sha256=[Convert]::ToHexString($hasher.GetHashAndReset()).ToLowerInvariant() }
        if ($VersionManifest) {
            $result.contentText = [Text.Encoding]::UTF8.GetString($manifest.ToArray()).TrimStart([char]0xFEFF)
        }
        return $result
    } catch [OperationCanceledException] {
        throw "Bitiful verification timed out for $RemoteKey. Publication outcome is unknown; no upload will be retried automatically."
    } finally {
        if ($stream) { $stream.Dispose() }
        if ($response) { $response.Dispose() }
        if ($manifest) { $manifest.Dispose() }
        $hasher.Dispose()
        $request.Dispose()
        $timeout.Dispose()
    }
}

function Get-ClipHostBitifulManifestVersion([string]$Json, [string]$RemoteKey) {
    try {
        $manifest = $Json | ConvertFrom-Json -AsHashtable -ErrorAction Stop
        $parsed = $null
        if ($manifest -isnot [Collections.IDictionary] -or
            [string]$manifest.version -notmatch '^\d+\.\d+\.\d+\.\d+$' -or
            ![version]::TryParse([string]$manifest.version, [ref]$parsed)) { throw 'Invalid version.' }
        return $parsed
    } catch { throw "Bitiful version manifest must contain a valid four-part version: $RemoteKey" }
}

function Get-ClipHostBitifulLocalObject {
    param([string]$LocalPath, [string]$ExpectedHash)
    $path = [IO.Path]::GetFullPath($LocalPath)
    $stream = [IO.File]::OpenRead($path)
    try {
        $hash = (Get-FileHash -InputStream $stream -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($ExpectedHash -notmatch '^[0-9a-fA-F]{64}$' -or $hash -cne $ExpectedHash.ToLowerInvariant()) {
            throw "Prepared Bitiful object checksum changed: $([IO.Path]::GetFileName($path))"
        }
        return @{ localPath=$path; sha256=$hash; sizeBytes=$stream.Length }
    } finally { $stream.Dispose() }
}

function Get-ClipHostBitifulObjectState {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][Net.Http.HttpClient]$Client,
        [Parameter(Mandatory)][string]$RemoteKey,
        [Parameter(Mandatory)][string]$LocalPath,
        [Parameter(Mandatory)][string]$ExpectedHash,
        [switch]$VersionManifest
    )
    $local = Get-ClipHostBitifulLocalObject -LocalPath $LocalPath -ExpectedHash $ExpectedHash
    $local.remoteKey = $RemoteKey
    if ($VersionManifest) {
        $version = Get-ClipHostBitifulManifestVersion -Json ([IO.File]::ReadAllText($local.localPath)) -RemoteKey $RemoteKey
        $local.version = $version.ToString(4)
    }
    $remote = Read-ClipHostBitifulObject -Client $Client -RemoteKey $RemoteKey -VersionManifest:$VersionManifest
    if ($remote.missing) { $local.status = 'missing' }
    elseif ($remote.sha256 -ceq $local.sha256 -and $remote.sizeBytes -eq $local.sizeBytes) {
        $local.status = 'same'
    } elseif (!$VersionManifest) {
        throw "Remote archive has different content; refusing to overwrite $RemoteKey."
    } else {
        $remoteVersion = Get-ClipHostBitifulManifestVersion -Json $remote.contentText -RemoteKey $RemoteKey
        if ($remoteVersion -gt $version) {
            throw "Remote Bitiful manifest is newer; refusing to downgrade $RemoteKey from $remoteVersion to $version."
        }
        if ($remoteVersion -eq $version) {
            throw "Remote Bitiful manifest has different content at the same version; refusing to overwrite $RemoteKey."
        }
        $local.status = 'update'
    }
    return [pscustomobject]$local
}

function Assert-ClipHostBitifulObjectHash {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][Net.Http.HttpClient]$Client,
        [Parameter(Mandatory)][string]$RemoteKey,
        [Parameter(Mandatory)][string]$LocalPath,
        [Parameter(Mandatory)][string]$ExpectedHash
    )
    $local = Get-ClipHostBitifulLocalObject -LocalPath $LocalPath -ExpectedHash $ExpectedHash
    $remote = Read-ClipHostBitifulObject -Client $Client -RemoteKey $RemoteKey
    if ($remote.missing -or $remote.sha256 -cne $local.sha256 -or $remote.sizeBytes -ne $local.sizeBytes) {
        throw "Bitiful upload could not be verified: $RemoteKey. Publication outcome is unknown; no upload will be retried automatically."
    }
}
