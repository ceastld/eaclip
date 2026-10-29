#requires -Version 5.1
[CmdletBinding()]
param(
    [string]$ExecutablePath,
    [ValidateRange(3, 45)][int]$WaitSeconds = 15,
    [switch]$NoOpen
)

# This file is also distributed beside cliphost-app.exe. Keep it compatible
# with the inbox Windows PowerShell 5.1; do not require a developer checkout.
Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

function Write-DiagnosticJson($Value, [string]$Path) {
    $json = ConvertTo-Json -InputObject $Value -Depth 12
    [IO.File]::WriteAllText($Path, $json, [Text.UTF8Encoding]::new($true))
}

function Copy-DiagnosticStreamSnapshot([IO.Stream]$Source, [IO.Stream]$Destination) {
    # Copy only the length observed at open time. A live process may still be
    # appending output; never wait for it to close or chase an unbounded stream.
    $remaining = $Source.Length - $Source.Position
    $buffer = New-Object byte[] 65536
    while ($remaining -gt 0) {
        $read = $Source.Read($buffer, 0, [int][Math]::Min($buffer.Length, $remaining))
        if ($read -le 0) { break }
        $Destination.Write($buffer, 0, $read)
        $remaining -= $read
    }
}

function Copy-DiagnosticFileSnapshot([string]$Source, [string]$Destination) {
    $input = [IO.File]::Open($Source, [IO.FileMode]::Open, [IO.FileAccess]::Read, ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
    try {
        $output = [IO.File]::Create($Destination)
        try { Copy-DiagnosticStreamSnapshot $input $output } finally { $output.Dispose() }
    } finally { $input.Dispose() }
}

function New-DiagnosticZip([string]$Directory, [string]$ZipPath) {
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $file = [IO.File]::Create($ZipPath)
    try {
        $archive = New-Object IO.Compression.ZipArchive($file, [IO.Compression.ZipArchiveMode]::Create, $false)
        try {
            foreach ($item in @(Get-ChildItem -LiteralPath $Directory -File)) {
                $input = [IO.File]::Open($item.FullName, [IO.FileMode]::Open, [IO.FileAccess]::Read, ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
                try {
                    $entry = $archive.CreateEntry($item.Name, [IO.Compression.CompressionLevel]::Optimal)
                    $output = $entry.Open()
                    try { Copy-DiagnosticStreamSnapshot $input $output } finally { $output.Dispose() }
                } finally { $input.Dispose() }
            }
        } finally { $archive.Dispose() }
    } finally { $file.Dispose() }
}

function Get-DiagnosticPeArchitecture([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    $reader = New-Object IO.BinaryReader($stream)
    try {
        if ($reader.ReadUInt16() -ne 0x5A4D) { throw 'The executable has no MZ header.' }
        $stream.Position = 0x3C
        $peOffset = $reader.ReadUInt32()
        if ($peOffset -gt ($stream.Length - 6)) { throw 'The executable PE offset is invalid.' }
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x4550) { throw 'The executable has no PE signature.' }
        $machine = $reader.ReadUInt16()
        $architecture = switch ($machine) { 0xAA64 { 'arm64' } 0x8664 { 'x64' } 0x14C { 'x86' } default { 'unknown' } }
        return [ordered]@{ architecture = $architecture; machine = ('0x{0:X4}' -f $machine) }
    } finally { $reader.Dispose(); $stream.Dispose() }
}

function Get-DiagnosticNativeArchitecture {
    if (-not ('ClipHostDiagnostics.NativeSystem' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace ClipHostDiagnostics {
    public static class NativeSystem {
        [DllImport("kernel32.dll", SetLastError=true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool IsWow64Process2(IntPtr process, out ushort processMachine, out ushort nativeMachine);
        public static ushort Architecture() {
            ushort processMachine, nativeMachine;
            try { return IsWow64Process2(new IntPtr(-1), out processMachine, out nativeMachine) ? nativeMachine : (ushort)0; }
            catch (EntryPointNotFoundException) { return 0; }
        }
    }
}
'@
    }
    switch ([ClipHostDiagnostics.NativeSystem]::Architecture()) {
        0xAA64 { 'arm64' } 0x8664 { 'x64' } 0x14C { 'x86' } 0x1C4 { 'arm' } default { 'unknown' }
    }
}

function Assert-DiagnosticLocalPath([string]$Path) {
    $absolute = [IO.Path]::GetFullPath($Path)
    if ($absolute -match '(^|[\\/])(OneDrive[^\\/]*|Dropbox|iCloud[^\\/]*)($|[\\/])') {
        throw 'Diagnostics will not traverse a cloud synchronization directory.'
    }
    # Check each explicit ancestor before accessing the next component. Never
    # follow junctions or links while inspecting an executable or startup log.
    $root = [IO.Path]::GetPathRoot($absolute)
    $candidate = $root
    foreach ($part in @($absolute.Substring($root.Length).Split(@([char]'\', [char]'/'), [StringSplitOptions]::RemoveEmptyEntries))) {
        $candidate = Join-Path $candidate $part
        if (Test-Path -LiteralPath $candidate) {
            $item = Get-Item -LiteralPath $candidate -Force
            if ($item.Attributes -band (0x400 -bor 0x1000 -bor 0x40000 -bor 0x400000)) {
                throw 'Diagnostics will not follow a link, junction or offline/cloud placeholder.'
            }
        }
    }
}

function Get-DiagnosticProcesses {
    $result = @()
    foreach ($process in @(Get-Process -Name 'cliphost-app', 'cliphost-gpui' -ErrorAction SilentlyContinue)) {
        try {
            $path = $null
            $started = $null
            try { $path = $process.Path } catch { }
            try { $started = $process.StartTime.ToUniversalTime().ToString('o') } catch { }
            $result += [ordered]@{ pid = $process.Id; name = $process.ProcessName; path = $path; startedUtc = $started }
        } finally { $process.Dispose() }
    }
    return $result
}

function Get-DiagnosticExitHex([int]$Code) {
    $unsigned = [BitConverter]::ToUInt32([BitConverter]::GetBytes($Code), 0)
    return ('0x{0:X8}' -f $unsigned)
}

function Get-DiagnosticHealth([int]$Port, [int[]]$RelevantPids) {
    $response = $null
    try {
        $request = [Net.HttpWebRequest]::Create("http://127.0.0.1:$Port/api/health")
        $request.Proxy = $null
        $request.Timeout = 700
        $request.ReadWriteTimeout = 700
        $response = $request.GetResponse()
        $reader = New-Object IO.StreamReader($response.GetResponseStream())
        try { $health = $reader.ReadToEnd() | ConvertFrom-Json } finally { $reader.Dispose() }
        $properties = @($health.PSObject.Properties.Name)
        $owners = @()
        $ownerError = $null
        try {
            $owners = @(Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction Stop |
                Where-Object { $_.LocalAddress -in @('127.0.0.1', '::1') } |
                Select-Object -ExpandProperty OwningProcess -Unique)
        } catch { $ownerError = $_.Exception.Message }
        $service = $null
        $ready = $false
        $ok = $false
        if ($properties -contains 'ok') { $ok = $health.ok -eq $true }
        if ($properties -contains 'service') { $service = [string]$health.service }
        if ($properties -contains 'windowRuntimeReady') { $ready = $health.windowRuntimeReady -eq $true }
        $ownerVerified = $owners.Count -eq 1 -and $owners[0] -in $RelevantPids
        return [ordered]@{
            utc = [DateTime]::UtcNow.ToString('o'); reachable = $true; service = $service
            windowRuntimeReady = $ready; ownerPids = $owners; ownerVerified = $ownerVerified
            readyVerified = ($ownerVerified -and $ok -and $service -eq 'intellitools-cliphost-tauri' -and $ready)
            ownerError = $ownerError
        }
    } catch {
        return [ordered]@{ utc = [DateTime]::UtcNow.ToString('o'); reachable = $false; readyVerified = $false; error = $_.Exception.Message }
    } finally { if ($null -ne $response) { $response.Dispose() } }
}

function Test-DiagnosticEventMatch($Event, [string]$ExePath, [int[]]$RelevantPids) {
    # Match structured faulting application paths when supplied by Application
    # Error/WER; do not collect unrelated events merely because they share an ID.
    $xml = [xml]$Event.ToXml()
    $fields = @($xml.SelectNodes("//*[local-name()='EventData']/*[local-name()='Data']"))
    foreach ($field in $fields) {
        $value = [string]$field.InnerText
        if ($value -ieq $ExePath) { return $true }
    }
    $message = [string]$Event.Message
    if ($message.IndexOf($ExePath, [StringComparison]::OrdinalIgnoreCase) -ge 0) { return $true }
    # Older/localized WER records can expose only the application name. Require
    # a matching process ID as well, and exclude events without that evidence.
    $exeName = [IO.Path]::GetFileName($ExePath)
    $hasName = $false
    $hasPid = $false
    foreach ($field in $fields) {
        $value = [string]$field.InnerText
        if ($value -ieq $exeName) { $hasName = $true }
        if ($field.GetAttribute('Name') -match '^(ProcessId|ProcessID|FaultingProcessId)$') {
            $eventPid = 0
            if ($value -match '^0x[0-9a-f]+$') { $eventPid = [Convert]::ToInt32($value.Substring(2), 16) }
            elseif (-not [int]::TryParse($value, [ref]$eventPid)) { continue }
            if ($eventPid -in $RelevantPids) { $hasPid = $true }
        }
    }
    return $hasName -and $hasPid
}

function Invoke-ClipHostDiagnostics {
    if ($env:OS -ne 'Windows_NT') { throw 'ClipHost diagnostics requires Windows.' }
    $diagnosticsRoot = Join-Path $env:LOCALAPPDATA 'ClipHostRust\diagnostics'
    Assert-DiagnosticLocalPath $diagnosticsRoot
    New-Item -ItemType Directory -Path $diagnosticsRoot -Force | Out-Null
    $stamp = [DateTime]::Now.ToString('yyyyMMdd-HHmmss-fff') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 6)
    $output = Join-Path $diagnosticsRoot ('report-' + $stamp)
    New-Item -ItemType Directory -Path $output | Out-Null
    $started = [DateTime]::UtcNow
    $report = [ordered]@{
        format = 'cliphost-startup-diagnostics-v1'; startedUtc = $started.ToString('o')
        powershellVersion = $PSVersionTable.PSVersion.ToString(); nativeArchitecture = $null
        nativeArchitectureMethod = 'IsWow64Process2.nativeMachine; unknown if unavailable'
        osVersion = [Environment]::OSVersion.VersionString; is64BitProcess = [Environment]::Is64BitProcess
        executable = $null; before = @(); observations = @(); health = @(); startedPid = $null
        launchSkipped = $false; launchError = $null; exited = $false; exitCode = $null; exitCodeHex = $null
        startupLogs = @(); startupLogNote = $null; events = @(); eventReadError = $null
        status = 'diagnostic-not-completed'; errors = @()
    }
    $process = $null
    $knownPids = New-Object 'Collections.Generic.HashSet[int]'
    $verifiedPids = New-Object 'Collections.Generic.HashSet[int]'
    try {
        $report.nativeArchitecture = Get-DiagnosticNativeArchitecture
        $exe = $ExecutablePath
        if ([string]::IsNullOrWhiteSpace($exe)) {
            $exe = Join-Path $PSScriptRoot 'cliphost-app.exe'
            if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
                $exe = Join-Path $env:LOCALAPPDATA 'Programs\ClipHostRust\cliphost-app.exe'
            }
        }
        $exe = [IO.Path]::GetFullPath($exe)
        Assert-DiagnosticLocalPath $exe
        $file = Get-Item -LiteralPath $exe -Force
        if ($file.Attributes -band (0x1000 -bor 0x40000 -bor 0x400000)) { throw 'The executable is an offline/cloud placeholder; diagnostics will not download it.' }
        if ($file.PSIsContainer -or $file.Extension -ine '.exe') { throw 'ExecutablePath must name an existing EXE.' }
        $version = [Diagnostics.FileVersionInfo]::GetVersionInfo($exe)
        $report.executable = [ordered]@{
            path = $exe; sizeBytes = $file.Length; sha256 = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash
            fileVersion = $version.FileVersion; productVersion = $version.ProductVersion
            pe = Get-DiagnosticPeArchitecture $exe
        }
        $before = @(Get-DiagnosticProcesses)
        $report.before = $before
        foreach ($item in $before) {
            if ($item.path -ieq $exe) {
                [void]$knownPids.Add([int]$item.pid)
                [void]$verifiedPids.Add([int]$item.pid)
            }
        }
        if ($before.Count -gt 0) {
            $report.launchSkipped = $true
            Write-Host 'ClipHost is already running. Collecting its current state without starting another instance.'
        } else {
            Write-Host 'Starting ClipHost once and collecting startup diagnostics...'
            try {
                $process = Start-Process -FilePath $exe -WorkingDirectory ([IO.Path]::GetDirectoryName($exe)) -WindowStyle Hidden -PassThru `
                    -RedirectStandardOutput (Join-Path $output 'stdout.log') -RedirectStandardError (Join-Path $output 'stderr.log')
                [void]$process.Handle
                $report.startedPid = $process.Id
                [void]$knownPids.Add($process.Id)
                [void]$verifiedPids.Add($process.Id)
            } catch { $report.launchError = $_.Exception.ToString() }
        }
        $port = 28965
        $override = [Environment]::GetEnvironmentVariable('INTELLITOOLS_CLIPHOST_TAURI_PORT')
        $parsedPort = 0
        if ([int]::TryParse($override, [ref]$parsedPort) -and $parsedPort -gt 0 -and $parsedPort -le 65535) { $port = $parsedPort }
        $report['apiPort'] = $port
        $deadline = [DateTime]::UtcNow.AddSeconds($WaitSeconds)
        if ($report.launchSkipped -or $null -eq $process) { $deadline = [DateTime]::UtcNow.AddSeconds(2) }
        do {
            $current = @(Get-DiagnosticProcesses)
            foreach ($item in $current) {
                if ($item.path -ieq $exe) {
                    [void]$knownPids.Add([int]$item.pid)
                    [void]$verifiedPids.Add([int]$item.pid)
                }
            }
            $report.observations += [ordered]@{ utc = [DateTime]::UtcNow.ToString('o'); processes = $current }
            $report.health += Get-DiagnosticHealth $port @($verifiedPids)
            if ($null -ne $process) {
                $process.Refresh()
                if ($process.HasExited) {
                    $process.WaitForExit()
                    $report.exited = $true
                    $report.exitCode = $process.ExitCode
                    if ($null -ne $process.ExitCode) { $report.exitCodeHex = Get-DiagnosticExitHex $process.ExitCode }
                    else { $report.errors += 'The process exited but its exit code was unavailable.' }
                    # Give Windows Error Reporting a short bounded interval to
                    # publish the crash, while observing any privilege relaunch.
                    $eventDeadline = [DateTime]::UtcNow.AddSeconds(3)
                    if ($deadline -gt $eventDeadline) { $deadline = $eventDeadline }
                }
            }
            Start-Sleep -Milliseconds 500
        } while ([DateTime]::UtcNow -lt $deadline)
        if ($report.exited) { $report.status = 'started-process-exited-see-exit-code-and-logs' }
        elseif ($report.launchError) { $report.status = 'process-launch-failed' }
        elseif ($report.launchSkipped) { $report.status = 'existing-process-observed-no-new-startup-tested' }
        else { $report.status = 'started-process-still-running-no-termination-requested' }
        foreach ($sourceFile in @(Get-ChildItem -LiteralPath $diagnosticsRoot -File -Filter 'startup-*.jsonl' -ErrorAction SilentlyContinue)) {
            if ($sourceFile.Name -match '^startup-([0-9]+)-([0-9]+)\.jsonl$') {
                $logMilliseconds = 0L
                if (-not [long]::TryParse($Matches[1], [ref]$logMilliseconds)) { continue }
                $id = [int]$Matches[2]
                if ($knownPids.Contains($id) -and -not ($sourceFile.Attributes -band (0x400 -bor 0x1000 -bor 0x40000 -bor 0x400000))) {
                    # Match only known PIDs and recent lifetime. No recursive
                    # collection, databases, clipboard files or configuration.
                    $earliest = $started.AddSeconds(-5)
                    $existing = @($before | Where-Object { $_.pid -eq $id -and $_.startedUtc })
                    if ($existing.Count) { $earliest = [DateTime]::Parse($existing[0].startedUtc).ToUniversalTime().AddSeconds(-5) }
                    $epoch = [DateTime]::SpecifyKind([DateTime]'1970-01-01', [DateTimeKind]::Utc)
                    $earliestMilliseconds = [long](($earliest - $epoch).TotalMilliseconds)
                    $latestMilliseconds = [long](([DateTime]::UtcNow.AddSeconds(5) - $epoch).TotalMilliseconds)
                    if ($sourceFile.LastWriteTimeUtc -ge $earliest -and $logMilliseconds -ge $earliestMilliseconds -and $logMilliseconds -le $latestMilliseconds) {
                        Copy-DiagnosticFileSnapshot $sourceFile.FullName (Join-Path $output $sourceFile.Name)
                        $report.startupLogs += $sourceFile.Name
                    }
                }
            }
        }
        if ($report.startupLogs.Count -eq 0) { $report.startupLogNote = 'No startup-PID log was captured. Older releases may not produce it; this does not prove startup succeeded.' }
        try {
            $events = @(Get-WinEvent -FilterHashtable @{ LogName = 'Application'; StartTime = $started.AddSeconds(-2).ToLocalTime(); EndTime = [DateTime]::Now; Id = @(1000, 1001, 1002, 1026) } -ErrorAction Stop)
            foreach ($event in $events) {
                if (Test-DiagnosticEventMatch $event $exe @($knownPids)) {
                    $report.events += [ordered]@{ utc = $event.TimeCreated.ToUniversalTime().ToString('o'); id = $event.Id; provider = $event.ProviderName; message = $event.Message; xml = $event.ToXml() }
                }
            }
        } catch {
            if ($_.FullyQualifiedErrorId -notlike 'NoMatchingEventsFound*') { $report.eventReadError = $_.Exception.Message }
        }
    } catch {
        $report.errors += $_.Exception.ToString()
        $report.status = 'diagnostic-collection-incomplete'
    } finally {
        if ($null -ne $process) { $process.Dispose() }
        $report['readyVerifiedObserved'] = @($report.health | Where-Object { $_.readyVerified -eq $true }).Count -gt 0
        $lastReady = $false
        if ($report.health.Count -gt 0) { $lastReady = $report.health[$report.health.Count - 1].readyVerified -eq $true }
        $report['lastReadyVerified'] = $lastReady
        $report['completedUtc'] = [DateTime]::UtcNow.ToString('o')
        Write-DiagnosticJson $report (Join-Path $output 'report.json')
    }
    $reportedPath = $ExecutablePath
    if ($null -ne $report.executable) { $reportedPath = $report.executable.path }
    $summary = @(
        'ClipHost startup diagnostics', ('Status: ' + $report.status), ('Executable: ' + $reportedPath),
        ('Native architecture: ' + $report.nativeArchitecture), ('Started PID: ' + $report.startedPid),
        ('Exit code: ' + $report.exitCode + ' / ' + $report.exitCodeHex),
        ('Last health readiness verified against the executable process: ' + $report.lastReadyVerified),
        ('Readiness verified at least once during observation: ' + $report.readyVerifiedObserved),
        'No existing processes were stopped. A running application is left running.',
        'Only startup output, executable identity, health readiness and associated Application errors are collected.',
        'Clipboard contents, history databases and configuration files are not collected. Nothing is uploaded.',
        'Missing startup logs or Windows events do not prove there was no failure.'
    )
    if ($report.launchError) { $summary += ('Launch error: ' + $report.launchError) }
    if ($report.errors.Count -gt 0) { $summary += ('Collection error: ' + $report.errors[0]) }
    $stderrPath = Join-Path $output 'stderr.log'
    if (Test-Path -LiteralPath $stderrPath -PathType Leaf) {
        $stderr = [IO.File]::Open($stderrPath, [IO.FileMode]::Open, [IO.FileAccess]::Read, ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
        try {
            $reader = New-Object IO.StreamReader($stderr)
            try {
                for ($lineNumber = 0; $lineNumber -lt 3; $lineNumber++) {
                    $line = $reader.ReadLine()
                    if ($null -eq $line) { break }
                    if ($line.Length -gt 500) { $line = $line.Substring(0, 500) + '...' }
                    $summary += ('Startup error output: ' + $line)
                }
            } finally { $reader.Dispose() }
        } finally { $stderr.Dispose() }
    }
    [IO.File]::WriteAllLines((Join-Path $output 'README.txt'), $summary, [Text.UTF8Encoding]::new($true))
    $zip = $output + '.zip'
    New-DiagnosticZip $output $zip
    Write-Host ''
    Write-Host ('Result: ' + $report.status)
    Write-Host ('Last health readiness verified: ' + $report.lastReadyVerified)
    if ($null -ne $report.exitCode) { Write-Host ('Exit code: ' + $report.exitCode + ' / ' + $report.exitCodeHex) }
    Write-Host ('Feedback ZIP: ' + $zip)
    Write-Host 'Send this ZIP to the developer to investigate the startup failure.'
    if (-not $NoOpen) {
        try { Start-Process -FilePath 'explorer.exe' -ArgumentList ('/select,"' + $zip + '"') -ErrorAction Stop | Out-Null }
        catch { Write-Host ('Could not open File Explorer. The ZIP remains at: ' + $zip) }
    }
    Write-Output $zip
}

if ($MyInvocation.InvocationName -ne '.') { Invoke-ClipHostDiagnostics }
