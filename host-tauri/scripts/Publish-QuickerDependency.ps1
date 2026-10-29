# Shared by Publish-Dependency.ps1. Dot-sourcing only defines functions; it never uploads.
function Invoke-ClipHostQuickerCli([string[]]$Arguments) {
    $cliPath = [Environment]::GetEnvironmentVariable('QUICKER_CLI')
    if (!$cliPath) {
        $command = Get-Command quicker -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        $cliPath = if ($command) { $command.Source } else {
            Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'quicker-cli/bin/quicker.exe'
        }
    }
    if (!(Test-Path -LiteralPath $cliPath -PathType Leaf)) {
        throw 'quicker CLI is required for official dependency publication; install it and run quicker login.'
    }
    $cliProcess = [Diagnostics.Process]::new()
    try {
        $cliProcess.StartInfo = [Diagnostics.ProcessStartInfo]::new($cliPath)
        $cliProcess.StartInfo.UseShellExecute = $false
        $cliProcess.StartInfo.CreateNoWindow = $true
        $cliProcess.StartInfo.RedirectStandardOutput = $true
        $cliProcess.StartInfo.RedirectStandardError = $true
        foreach ($argument in $Arguments) { $cliProcess.StartInfo.ArgumentList.Add($argument) }
        [void]$cliProcess.Start()
        $output = $cliProcess.StandardOutput.ReadToEndAsync()
        $errors = $cliProcess.StandardError.ReadToEndAsync()
        if (!$cliProcess.WaitForExit(180000)) {
            $cliProcess.Kill($true)
            throw 'quicker CLI timed out. Publication outcome is unknown; no upload will be retried automatically.'
        }
        [void]$errors.GetAwaiter().GetResult()
        if ($cliProcess.ExitCode -ne 0) {
            throw "quicker CLI failed (exit $($cliProcess.ExitCode)). Check the official version before retrying; no upload was retried."
        }
        $output.GetAwaiter().GetResult()
    } finally { $cliProcess.Dispose() }
}

function Get-ClipHostQuickerPackage([string]$Name, [string]$Version, [switch]$MinimumVersion) {
    $output = Invoke-ClipHostQuickerCli @('dep', 'find', '--name', $Name, '--version', $Version)
    try { $result = $output | ConvertFrom-Json -AsHashtable -ErrorAction Stop }
    catch { throw "Invalid official dependency lookup response for $Name; publication cancelled." }
    if ($result.isSuccess -eq $true) {
        # dep find returns the newest package satisfying the requested minimum.
        # Only launcher availability uses a minimum; publishing a specific ZIP
        # still requires an exact identity before its content can be compared.
        $versionMatches = $result.data.version -ceq $Version
        if ($MinimumVersion) {
            $versionMatches = [string]$result.data.version -match '^\d+\.\d+\.\d+$' -and
                [version]$result.data.version -ge [version]$Version
        }
        if ($result.data.name -cne $Name -or !$versionMatches -or
            @($result.data.downloadLinks).Count -eq 0) {
            throw "Official dependency identity or download link does not match $Name $Version."
        }
        return $result.data
    }
    if ($result.isSuccess -eq $false -and $result.errorCode -eq 1 -and
        $null -eq $result.data -and
        @('未找到符合条件的包', '未找到符合版本要求的包。') -ccontains $result.message) { return $null }
    throw "Could not determine whether $Name $Version exists; publication cancelled."
}

function Test-ClipHostQuickerPackageHash($Package, [string]$ExpectedHash) {
    $uri = [Uri]::new([string]$Package.downloadLinks[0], [UriKind]::Absolute)
    if ($uri.Scheme -cne 'https') { throw 'Official dependency download must use HTTPS.' }
    $client = [Net.Http.HttpClient]::new()
    $timeout = [Threading.CancellationTokenSource]::new([TimeSpan]::FromSeconds(180))
    $response = $null
    try {
        $response = $client.GetAsync($uri, [Net.Http.HttpCompletionOption]::ResponseHeadersRead,
            $timeout.Token).GetAwaiter().GetResult()
        [void]$response.EnsureSuccessStatusCode()
        $stream = $response.Content.ReadAsStreamAsync($timeout.Token).GetAwaiter().GetResult()
        $hasher = [Security.Cryptography.SHA256]::Create()
        try {
            $hashBytes = $hasher.ComputeHashAsync($stream, $timeout.Token).GetAwaiter().GetResult()
            $actualHash = [Convert]::ToHexString($hashBytes).ToLowerInvariant()
        } finally { $hasher.Dispose(); $stream.Dispose() }
        if ($actualHash -cne $ExpectedHash) {
            throw "Official dependency $($Package.name) $($Package.version) has different ZIP content; refusing to overwrite."
        }
    } finally {
        if ($response) { $response.Dispose() }
        $timeout.Dispose()
        $client.Dispose()
    }
}

function Publish-ClipHostQuickerDependency([string]$PublicationDirectory) {
    $root = [IO.Path]::GetFullPath($PublicationDirectory).TrimEnd('\', '/')
    $receipt = Get-Content -LiteralPath (Join-Path $root 'publication.json') -Raw | ConvertFrom-Json -AsHashtable
    if ($receipt.version -notmatch '^\d+\.\d+\.\d+\.\d+$') { throw 'Invalid prepared publication version.' }
    # Older frozen receipts used launcherVersion for both values. Do not rewrite them.
    $requiredLauncher = if ($receipt.ContainsKey('requiredLauncherVersion')) {
        [string]$receipt.requiredLauncherVersion
    } else { [string]$receipt.launcherVersion }
    if ([string]$receipt.launcherVersion -notmatch '^\d+\.\d+\.\d+\.\d+$' -or
        $requiredLauncher -notmatch '^\d+\.\d+\.\d+\.\d+$' -or
        [version]$requiredLauncher -gt [version]$receipt.launcherVersion) {
        throw 'Invalid prepared launcher compatibility version.'
    }
    $version = ($receipt.version.Split('.')[0..2] -join '.')
    $descriptions = @{
        'cea.cliphost.rust' = 'ClipHost Rust 独立剪贴板宿主。'
        'cea.cliphost.rust.arm64' = 'ClipHost Rust ARM64 独立剪贴板宿主。'
        'cea.clip.quicker.rust' = 'ClipHost Rust 的 Quicker V1 启动器。'
        'cea.clip.quicker.rust.net10' = 'ClipHost Rust 的 Quicker V2 启动器。'
    }
    $archives = [Collections.Generic.List[object]]::new()
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    try {
        # Verify every frozen file before the first POST, and hold ZIPs open against writes.
        # Also order receipts prepared by older host-first publishers.
        foreach ($package in ($receipt.packages | Sort-Object {
            switch ($_.packageName) {
                'cea.clip.quicker.rust' { 0 }
                'cea.clip.quicker.rust.net10' { 1 }
                default { 2 }
            }
        })) {
            $name = [string]$package.packageName
            if (@($descriptions.Keys) -cnotcontains $name -or !$seen.Add($name) -or $package.version -cne $receipt.version) {
                throw 'Unexpected, duplicate, or mismatched prepared dependency package.'
            }
            $zipCount = 0
            foreach ($file in $package.uploads) {
                $path = [IO.Path]::GetFullPath([string]$file.localPath)
                $fileName = [IO.Path]::GetFileName($path)
                if (!$path.StartsWith($root + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase) -or
                    $file.remoteKey -cne "$name/$fileName") { throw 'Prepared dependency path changed.' }
                $baseName = if ($name -in @('cea.cliphost.rust', 'cea.cliphost.rust.arm64')) { 'ClipHostRust' } else { 'IntelliTools' }
                if ($fileName -ceq "$baseName.$($receipt.version).zip") {
                    $stream = [IO.File]::Open($path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
                    $archive = @{ name=$name; path=$path; sha256=[string]$file.sha256; stream=$stream; existing=$null }
                    $archives.Add($archive)
                    $hash = (Get-FileHash -InputStream $stream -Algorithm SHA256).Hash.ToLowerInvariant()
                    $zipCount++
                } elseif ($fileName -ceq 'version.json') {
                    $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
                } else { throw 'Unexpected prepared dependency file.' }
                if ($hash -cne $file.sha256) { throw "Prepared dependency checksum changed: $name/$fileName" }
            }
            if ($zipCount -ne 1) { throw "Expected one frozen ZIP for $name." }
        }
        $launcherOnly = $receipt.ContainsKey('publicationKind') -and $receipt.publicationKind -ceq 'launchers'
        if ($launcherOnly) {
            if ($seen.Count -ne 2 -or !$seen.Contains('cea.clip.quicker.rust') -or !$seen.Contains('cea.clip.quicker.rust.net10')) {
                throw 'A launcher-only publication must include exactly both launcher packages.'
            }
        } elseif (!$seen.Contains('cea.cliphost.rust') -and !$seen.Contains('cea.cliphost.rust.arm64')) {
            throw 'Prepared publication must include the Rust host.'
        }
        # Host-only releases reuse a previously published compatible launcher pair.
        $requiredDependency = ($requiredLauncher.Split('.')[0..2] -join '.')
        foreach ($launcher in @('cea.clip.quicker.rust', 'cea.clip.quicker.rust.net10')) {
            if (!$seen.Contains($launcher) -and !(Get-ClipHostQuickerPackage $launcher $requiredDependency -MinimumVersion)) {
                throw "Required launcher dependency is not published: $launcher $requiredDependency"
            }
        }
        # Reject any conflicting existing version before publishing another package.
        foreach ($archive in $archives) {
            $archive.existing = Get-ClipHostQuickerPackage $archive.name $version
            if ($archive.existing) { Test-ClipHostQuickerPackageHash $archive.existing $archive.sha256 }
        }
        $results = [Collections.Generic.List[object]]::new()
        foreach ($archive in $archives) {
            $status = 'already-present'
            if (!$archive.existing) {
                # Exactly one upload invocation. An unknown outcome stops the publication.
                [void](Invoke-ClipHostQuickerCli @('dep', 'upload', '--file', $archive.path,
                    '--name', $archive.name, '--version', $version, '--description', $descriptions[$archive.name], '--public'))
                $archive.existing = Get-ClipHostQuickerPackage $archive.name $version
                if (!$archive.existing) { throw 'Official upload could not be confirmed; no upload will be retried automatically.' }
                Test-ClipHostQuickerPackageHash $archive.existing $archive.sha256
                $status = 'uploaded'
            }
            $results.Add([ordered]@{ packageName=$archive.name; version=$version; status=$status;
                sha256=$archive.sha256; downloadUrl=$archive.existing.downloadLinks[0] })
            Write-Host "Official dependency verified: $($archive.name) $version ($status)"
            [ordered]@{ version=$version; packages=@($results.ToArray()) } | ConvertTo-Json -Depth 6 |
                Set-Content -LiteralPath (Join-Path $root 'quicker-upload.json') -Encoding utf8NoBOM
        }
    } finally { foreach ($archive in $archives) { $archive.stream.Dispose() } }
}
