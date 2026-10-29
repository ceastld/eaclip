#requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateNotNullOrEmpty()][string[]]$Path,
    [Parameter(Mandatory)][ValidateNotNullOrEmpty()][string]$OutputDirectory,
    [ValidateRange(1, 300)][int]$TimeoutSeconds = 300
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (![OperatingSystem]::IsWindows()) { throw 'Microsoft Defender scanning requires Windows.' }

$clipRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$temporaryRoot = Join-Path $clipRoot '.tmp'
# Offline, RecallOnOpen, RecallOnDataAccess, and every reparse point (including
# symlinks and junctions). Inspect ancestors before reading file contents.
$unsafeAttributes = 0x1000 -bor 0x40000 -bor 0x400000 -bor 0x400
function Assert-LocalPath([string]$LiteralPath, [switch]$AllowMissing) {
    if ([string]::IsNullOrWhiteSpace($LiteralPath)) { throw 'An explicit local path is required.' }
    $full = [IO.Path]::GetFullPath($LiteralPath)
    if ($full.StartsWith('\\')) { throw "Network paths are not supported: $full" }
    $current = $full
    while ($current) {
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force
            if (([long]$item.Attributes -band $unsafeAttributes) -ne 0) {
                throw "Cloud placeholder or reparse path is not allowed: $current"
            }
        } elseif (!$AllowMissing) {
            throw "Path does not exist: $current"
        }
        $parent = [IO.Directory]::GetParent($current)
        $current = if ($parent) { $parent.FullName } else { $null }
    }
    return $full
}

function Read-DefenderStatus {
    $status = Get-MpComputerStatus -ErrorAction Stop
    if (!$status.AMServiceEnabled -or !$status.AntivirusEnabled -or
        [string]::IsNullOrWhiteSpace($status.AntivirusSignatureVersion)) {
        throw 'Microsoft Defender Antivirus or its signatures are unavailable.'
    }
    return [ordered]@{
        productVersion = $status.AMProductVersion
        serviceVersion = $status.AMServiceVersion
        engineVersion = $status.AMEngineVersion
        signatureVersion = $status.AntivirusSignatureVersion
        signatureLastUpdatedUtc = $status.AntivirusSignatureLastUpdated.ToUniversalTime().ToString('o')
        serviceEnabled = [bool]$status.AMServiceEnabled
        antivirusEnabled = [bool]$status.AntivirusEnabled
    }
}

function Find-DefenderCommand {
    $candidates = [Collections.Generic.List[string]]::new()
    $platform = Join-Path $env:ProgramData 'Microsoft/Windows Defender/Platform'
    if (Test-Path -LiteralPath $platform) {
        $platform = Assert-LocalPath $platform
        $versions = @(Get-ChildItem -LiteralPath $platform -Directory -Force |
            Where-Object { ([long]$_.Attributes -band $unsafeAttributes) -eq 0 -and $_.Name -match '^\d+\.\d+\.\d+\.\d+-\d+$' } |
            Sort-Object { [version]($_.Name.Split('-')[0]) } -Descending)
        foreach ($version in $versions) { $candidates.Add((Join-Path $version.FullName 'MpCmdRun.exe')) }
    }
    $programFilesRoot = if ($env:ProgramW6432) { $env:ProgramW6432 } else { $env:ProgramFiles }
    $candidates.Add((Join-Path $programFilesRoot 'Windows Defender/MpCmdRun.exe'))
    foreach ($candidate in $candidates) {
        if (!(Test-Path -LiteralPath $candidate -PathType Leaf)) { continue }
        $candidate = Assert-LocalPath $candidate
        $signature = Get-AuthenticodeSignature -LiteralPath $candidate
        if ($signature.Status -ne 'Valid' -or !$signature.SignerCertificate -or
            $signature.SignerCertificate.Subject -notmatch '(^|,\s*)O=Microsoft Corporation(,|$)') { continue }
        return $candidate
    }
    throw 'No valid Microsoft-signed MpCmdRun.exe was found in the Defender Platform or Program Files directories.'
}

$output = Assert-LocalPath $OutputDirectory -AllowMissing
if (!$output.StartsWith($temporaryRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw "OutputDirectory must be a new directory beneath $temporaryRoot"
}
if (Test-Path -LiteralPath $output) { throw "OutputDirectory already exists: $output" }
# Do not overwrite an earlier receipt, even if a previous run failed.
New-Item -ItemType Directory -Path $output -ErrorAction Stop | Out-Null
$output = Assert-LocalPath $output
$reportPath = Join-Path $output 'report.json'
$runLog = Join-Path $output 'verification.log'
$report = [ordered]@{
    schema = 'cliphost-release-antivirus-v1'
    startedAtUtc = [DateTime]::UtcNow.ToString('o')
    completedAtUtc = $null
    passed = $false
    scanner = 'Microsoft Defender Antivirus'
    requestedPaths = @($Path)
    timeoutSeconds = $TimeoutSeconds
    defender = $null
    scans = @()
    error = $null
}
Set-Content -LiteralPath $runLog -Value "Started $($report.startedAtUtc)" -Encoding utf8NoBOM
$failure = $null
try {
    $files = @($Path | ForEach-Object {
        $file = Assert-LocalPath $_
        if (!(Test-Path -LiteralPath $file -PathType Leaf)) { throw "Only explicit files are accepted; directories are rejected: $file" }
        $file
    })
    if (@($files | Sort-Object -Unique).Count -ne $files.Count) { throw 'Duplicate input files are not accepted.' }
    $command = Find-DefenderCommand
    $commandInfo = Get-Item -LiteralPath $command
    $report.defender = [ordered]@{
        command = $command
        commandFileVersion = $commandInfo.VersionInfo.FileVersion
        commandSha256 = (Get-FileHash -LiteralPath $command -Algorithm SHA256).Hash
        commandAuthenticodeStatus = 'Valid'
        before = Read-DefenderStatus
        after = $null
    }
    for ($index = 0; $index -lt $files.Count; $index++) {
        $file = Assert-LocalPath $files[$index]
        $entry = [ordered]@{
            path = $file
            startedAtUtc = [DateTime]::UtcNow.ToString('o')
            completedAtUtc = $null
            passed = $false
            sha256Before = $null
            sha256After = $null
            lengthBefore = $null
            lengthAfter = $null
            fileVersion = $null
            productVersion = $null
            authenticode = $null
            arguments = @('-Scan', '-ScanType', '3', '-File', $file, '-DisableRemediation')
            exitCode = $null
            timedOut = $false
            stdoutLog = Join-Path $output ('scan-{0:D3}.stdout.log' -f ($index + 1))
            stderrLog = Join-Path $output ('scan-{0:D3}.stderr.log' -f ($index + 1))
            error = $null
        }
        $report.scans += $entry
        $lockedFile = $null
        $process = $null
        $scanStarted = $false
        try {
            # Keep input immutable throughout metadata collection and scanning.
            $lockedFile = [IO.File]::Open($file, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
            $entry.sha256Before = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash
            $entry.lengthBefore = $lockedFile.Length
            $info = Get-Item -LiteralPath $file
            $entry.fileVersion = $info.VersionInfo.FileVersion
            $entry.productVersion = $info.VersionInfo.ProductVersion
            $signature = Get-AuthenticodeSignature -LiteralPath $file
            $entry.authenticode = [ordered]@{
                status = $signature.Status.ToString()
                statusMessage = $signature.StatusMessage
                signerSubject = if ($signature.SignerCertificate) { $signature.SignerCertificate.Subject } else { $null }
                signerThumbprint = if ($signature.SignerCertificate) { $signature.SignerCertificate.Thumbprint } else { $null }
            }
            Set-Content -LiteralPath $entry.stdoutLog -Value '' -NoNewline -Encoding utf8NoBOM
            Set-Content -LiteralPath $entry.stderrLog -Value '' -NoNewline -Encoding utf8NoBOM
            $start = [Diagnostics.ProcessStartInfo]::new()
            $start.FileName = $command
            $start.UseShellExecute = $false
            $start.CreateNoWindow = $true
            $start.RedirectStandardOutput = $true
            $start.RedirectStandardError = $true
            foreach ($argument in $entry.arguments) { $start.ArgumentList.Add($argument) }
            $process = [Diagnostics.Process]::new()
            $process.StartInfo = $start
            if (!$process.Start()) { throw 'The Defender scan process did not start.' }
            $scanStarted = $true
            $stdout = $process.StandardOutput.ReadToEndAsync()
            $stderr = $process.StandardError.ReadToEndAsync()
            if (!$process.WaitForExit($TimeoutSeconds * 1000)) {
                $entry.timedOut = $true
                # This exact process was created above. Never cancel other scans,
                # stop the Defender service, or change antivirus configuration.
                $process.Kill()
                if (!$process.WaitForExit(5000)) { throw 'The timed-out scan process could not be stopped.' }
            }
            if (!$stdout.Wait(5000) -or !$stderr.Wait(5000)) { throw 'Defender scan logs did not finish within their capture deadline.' }
            Set-Content -LiteralPath $entry.stdoutLog -Value $stdout.Result -NoNewline -Encoding utf8NoBOM
            Set-Content -LiteralPath $entry.stderrLog -Value $stderr.Result -NoNewline -Encoding utf8NoBOM
            $entry.exitCode = $process.ExitCode
            if ($entry.timedOut) { throw "Defender scan exceeded $TimeoutSeconds seconds." }
            if ($entry.exitCode -ne 0) { throw "Defender scan failed or found a threat; exit code $($entry.exitCode). See its logs." }
            $entry.passed = $true
        } catch {
            $entry.error = $_.Exception.Message
        } finally {
            if ($scanStarted -and !$process.HasExited) {
                try {
                    $process.Kill()
                    if (!$process.WaitForExit(5000)) { throw 'The owned scan process did not exit after termination.' }
                } catch {
                    $entry.passed = $false
                    $entry.error = (@($entry.error, $_.Exception.Message) | Where-Object { $_ }) -join '; '
                }
            }
            try {
                Assert-LocalPath $file | Out-Null
                $entry.sha256After = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash
                $entry.lengthAfter = (Get-Item -LiteralPath $file).Length
                if (!$entry.sha256Before -or $entry.sha256Before -cne $entry.sha256After -or $entry.lengthBefore -ne $entry.lengthAfter) {
                    throw 'The input file changed or could not be verified across scanning.'
                }
            } catch {
                $entry.passed = $false
                $entry.error = (@($entry.error, $_.Exception.Message) | Where-Object { $_ }) -join '; '
            }
            if ($process) { $process.Dispose() }
            if ($lockedFile) { $lockedFile.Dispose() }
            $entry.completedAtUtc = [DateTime]::UtcNow.ToString('o')
            Add-Content -LiteralPath $runLog -Value "$($entry.path): passed=$($entry.passed); exit=$($entry.exitCode); error=$($entry.error)" -Encoding utf8NoBOM
        }
    }
    $report.defender.after = Read-DefenderStatus
    if (@($report.scans | Where-Object { !$_.passed }).Count -gt 0) { throw 'One or more explicit files did not pass the Defender scan.' }
    $report.passed = $report.scans.Count -eq $files.Count -and $report.scans.Count -gt 0
    if (!$report.passed) { throw 'No complete Defender scan was recorded.' }
} catch {
    $failure = $_.Exception.Message
    $report.error = $failure
    Add-Content -LiteralPath $runLog -Value "FAILED: $failure" -Encoding utf8NoBOM
} finally {
    $report.completedAtUtc = [DateTime]::UtcNow.ToString('o')
    $report | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $reportPath -Encoding utf8NoBOM
}
Write-Output $reportPath
if ($failure) { throw $failure }
