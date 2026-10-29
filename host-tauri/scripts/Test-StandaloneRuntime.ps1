[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$AssetDirectory,
    [Parameter(Mandatory)][ValidateSet('x64', 'arm64')][string]$ExpectedArchitecture,
    [Parameter(Mandatory)][ValidatePattern('^\d+\.\d+\.\d+$')][string]$ExpectedVersion,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [ValidateRange(1024, 65535)][int]$ApiPort = 29065
)

# Reuses the 0.31.13 ARM64 frozen-artifact smoke: read the original ZIP as
# published, then start only its extracted EXE with fresh, isolated test data.
# No install, distribution, system settings or production clipboard is used.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (!$IsWindows) { throw 'Native standalone runtime validation requires Windows PowerShell 7.' }
. (Join-Path $PSScriptRoot 'Read-PortableExecutable.ps1')
. (Join-Path $PSScriptRoot 'Read-NativeSymbols.ps1')
. (Join-Path $PSScriptRoot 'Read-ExecutableBuildInfo.ps1')
. (Join-Path $PSScriptRoot 'Assert-StandaloneBuildInfo.ps1')
if ($ApiPort -in @(28965, 28865, 18765, 18776, 28766)) { throw 'Use a separate test API port.' }
if ([Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() -ine $ExpectedArchitecture -or
    [Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString() -ine $ExpectedArchitecture) {
    throw 'Run runtime smoke natively on the package architecture, including PowerShell.'
}
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$scratch = [IO.Path]::GetFullPath((Join-Path $repo '.tmp'))
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
if (!$OutputDirectory.StartsWith($scratch + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase) -or
    (Test-Path -LiteralPath $OutputDirectory)) { throw 'Runtime smoke requires a new directory under this repository .tmp.' }
$ancestor = [IO.DirectoryInfo]::new($OutputDirectory).Parent
while (!$ancestor.Exists) { $ancestor = $ancestor.Parent }
$resolvedAncestor = (Resolve-Path -LiteralPath $ancestor.FullName).ProviderPath
if ($resolvedAncestor -ine $scratch -and !$resolvedAncestor.StartsWith($scratch + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Runtime smoke output must remain under the repository .tmp.'
}
for ($checkedAncestor = $ancestor; $checkedAncestor.FullName.StartsWith($scratch, [StringComparison]::OrdinalIgnoreCase); $checkedAncestor = $checkedAncestor.Parent) {
    if ((Get-Item -LiteralPath $checkedAncestor.FullName -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw 'Runtime smoke output ancestors must not be links or junctions.'
    }
}
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$AssetDirectory = [IO.Path]::GetFullPath($AssetDirectory)
$stem = "eaclip-$ExpectedVersion-windows-$ExpectedArchitecture"
$zip = Join-Path $AssetDirectory "$stem.zip"
$manifestPath = Join-Path $AssetDirectory "$stem.build-info.json"
$hashes = @{}
foreach ($line in Get-Content -LiteralPath (Join-Path $AssetDirectory "$stem.SHA256SUMS.txt")) {
    if ($line -cnotmatch '^([0-9a-f]{64})  ([^/\\]+)$' -or
        $Matches[2] -cnotin @("$stem.zip", "$stem.build-info.json") -or $hashes.ContainsKey($Matches[2])) {
        throw 'Invalid or duplicate portable asset checksum entry.'
    }
    $hashes[$Matches[2]] = $Matches[1]
}
if ($hashes.Count -ne 2) { throw 'Incomplete portable asset checksum list.' }
foreach ($name in $hashes.Keys) {
    if ((Get-FileHash -LiteralPath (Join-Path $AssetDirectory $name) -Algorithm SHA256).Hash.ToLowerInvariant() -cne $hashes[$name]) {
        throw "Portable asset checksum mismatch: $name"
    }
}
$info = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json -AsHashtable
$target = if ($ExpectedArchitecture -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
# The original 0.31.13 x64 manifest stored target only inside executableBuild.
# Actual PE and embedded executable identity are independently checked below.
$manifestTarget = if ($info.Contains('target')) { $info.target } else { $info.executableBuild.target }
if ($info.product -cne 'ClipHost Rust' -or $info.version -cne "$ExpectedVersion.0" -or
    $info.architecture -cne $ExpectedArchitecture -or $manifestTarget -cne $target -or
    $info.renderer -cne 'gpui' -or $info.profile -cne 'release' -or $info.embeddedFrontend -ne $false -or
    $info.executable -cne 'cliphost-app.exe') { throw 'Portable asset metadata does not match the expected GPUI Release.' }
$allowed = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($name in @('build-info.json', 'SHA256SUMS.txt', 'README.txt') + @($info.files.name)) {
    if (!$name -or $name -match '[\\/:]' -or $name -in @('.', '..') -or !$allowed.Add($name)) { throw 'Invalid package payload name.' }
}
$archive = [IO.Compression.ZipFile]::OpenRead($zip)
try {
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($entry in $archive.Entries) {
        if ($entry.FullName.EndsWith('/')) {
            if ($entry.FullName -cne 'ClipHost-Rust/') { throw "Unexpected portable ZIP directory: $($entry.FullName)" }
            continue
        }
        if ($entry.FullName -cnotmatch '^ClipHost-Rust/([^/\\]+)$' -or !$allowed.Contains($Matches[1]) -or !$seen.Add($Matches[1])) {
            throw "Unexpected portable ZIP entry: $($entry.FullName)"
        }
    }
    if ($seen.Count -ne $allowed.Count) { throw 'Incomplete portable ZIP payload.' }
} finally { $archive.Dispose() }
$extracted = Join-Path $OutputDirectory 'extracted'
[IO.Compression.ZipFile]::ExtractToDirectory($zip, $extracted)
$package = Join-Path $extracted 'ClipHost-Rust'
if ((Get-FileHash -LiteralPath (Join-Path $package 'build-info.json')).Hash.ToLowerInvariant() -cne $hashes["$stem.build-info.json"]) {
    throw 'The ZIP manifest differs from the frozen external manifest.'
}
$innerHashes = @{}
foreach ($line in Get-Content -LiteralPath (Join-Path $package 'SHA256SUMS.txt')) {
    if ($line -cnotmatch '^([0-9a-f]{64})  ([^/\\]+)$' -or $innerHashes.ContainsKey($Matches[2])) { throw 'Invalid package checksum list.' }
    $innerHashes[$Matches[2]] = $Matches[1]
}
if ($innerHashes.Count -ne @($info.files).Count) { throw 'Incomplete package checksum list.' }
$payloads = @(foreach ($file in $info.files) {
    $path = Join-Path $package $file.name
    $item = Get-Item -LiteralPath $path
    $sha = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($item.Length -ne $file.sizeBytes -or $sha -cne $file.sha256 -or $innerHashes[$file.name] -cne $sha) {
        throw "Package payload size or checksum mismatch: $($file.name)"
    }
    $payloadArchitecture = $null
    if ($item.Extension -in @('.exe', '.dll')) {
        $pe = Read-ClipHostPortableExecutable -Path $path
        if ($pe.architecture -cne $ExpectedArchitecture) { throw "Native package payload mismatch: $($file.name)" }
        $payloadArchitecture = $pe.architecture
    } elseif ($item.Extension -ieq '.pdb') {
        $codeView = Read-ClipHostCodeViewIdentity -Path (Join-Path $package 'cliphost-app.exe')
        $symbolStream = [IO.File]::OpenRead($path)
        try { Assert-ClipHostNativePdb -CodeView $codeView -Stream $symbolStream -Name $file.name }
        finally { $symbolStream.Dispose() }
    } elseif ($file.name -cnotin @('Start-ClipHostDiagnostics.ps1', 'Start-ClipHostDiagnostics.cmd')) {
        throw "Unexpected nonbinary package payload: $($file.name)"
    }
    [ordered]@{ name=$file.name; sha256=$sha; architecture=$payloadArchitecture }
})
if (!($info.files | Where-Object name -CEQ 'cliphost-app.exe') -or !($info.files | Where-Object name -IEQ 'vcruntime140.dll')) {
    throw 'The package must contain its executable and VC runtime.'
}
$exe = Join-Path $package 'cliphost-app.exe'
$exeHash = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
$embedded = Read-ClipHostExecutableBuildInfo -Path $exe
Assert-ClipHostStandaloneBuildInfo -BuildInfo $embedded -Renderer Gpui
if ($embedded.version -cne $ExpectedVersion -or $embedded.target -cne $target -or $info.sha256 -cne $exeHash) {
    throw 'Actual executable version, target or checksum differs from the package.'
}
[ordered]@{ version=$ExpectedVersion; architecture=$ExpectedArchitecture; assets=$hashes; payloads=$payloads; embedded=$embedded } |
    ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'package-verification.json') -Encoding utf8NoBOM
$result = [ordered]@{
    executable=$exe; sha256=$exeHash; version=$ExpectedVersion; architecture=$ExpectedArchitecture
    loader=[ordered]@{ passed=$false; exitCode=$null; error=$null }
    headless=[ordered]@{ healthPassed=$false; normalSelfExit=$false; exitCode=$null; pid=$null; error=$null; lastHealthError=$null; tcpListeners=@(); udpListeners=@(); runtimeModules=@() }
    guiImeOleAcceptance=$false
}
$loader = $null
try {
    $report = Join-Path $OutputDirectory 'fixture-report.json'
    $loader = Start-Process -FilePath $exe -ArgumentList @('--fixture', '--inspect', '--report', ('"' + $report + '"')) -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput (Join-Path $OutputDirectory 'fixture.stdout.log') -RedirectStandardError (Join-Path $OutputDirectory 'fixture.stderr.log')
    if (!$loader.WaitForExit(30000)) { throw 'Fixture loader did not exit within 30 seconds.' }
    $result.loader.exitCode = $loader.ExitCode
    if ($loader.ExitCode -ne 0) { throw "Fixture native loader exited with $($loader.ExitCode)." }
    $fixture = Get-Content -LiteralPath $report -Raw | ConvertFrom-Json
    if ($fixture.mode -cne 'fixture' -or $fixture.productionHostStarted -ne $false -or $fixture.fixtureCount -lt 1) {
        throw 'Fixture report did not prove isolated inspection.'
    }
    $result.loader.passed = $true
} catch { $result.loader.error = $_.Exception.ToString() }
finally { if ($null -ne $loader -and !$loader.HasExited) { $loader.Kill(); [void]$loader.WaitForExit(5000) } }
$hostProcess = $null
if ($result.loader.passed) {
    try {
        if (Get-NetTCPConnection -LocalPort $ApiPort -State Listen -ErrorAction SilentlyContinue) { throw 'The test API port is already occupied.' }
        $data = Join-Path $OutputDirectory 'isolated-data'
        $report = Join-Path $OutputDirectory 'headless-report.json'
        $hostProcess = Start-Process -FilePath $exe -ArgumentList @('--test-instance', '--headless', '--background', '--data-root', ('"' + $data + '"'),
            '--api-port', $ApiPort, '--exit-after-ms', '15000', '--report', ('"' + $report + '"')) -WindowStyle Hidden -PassThru `
            -RedirectStandardOutput (Join-Path $OutputDirectory 'headless.stdout.log') -RedirectStandardError (Join-Path $OutputDirectory 'headless.stderr.log')
        $result.headless.pid = $hostProcess.Id
        $deadline = [DateTime]::UtcNow.AddSeconds(45)
        while ([DateTime]::UtcNow -lt $deadline -and !$hostProcess.HasExited) {
            $tcp = @(Get-NetTCPConnection -OwningProcess $hostProcess.Id -State Listen -ErrorAction SilentlyContinue)
            $udp = @(Get-NetUDPEndpoint -OwningProcess $hostProcess.Id -ErrorAction SilentlyContinue)
            if ($tcp.Count) { $result.headless.tcpListeners = @($tcp | Select-Object LocalAddress,LocalPort,OwningProcess) }
            if ($udp.Count) { $result.headless.udpListeners = @($udp | Select-Object LocalAddress,LocalPort,OwningProcess) }
            if (@($tcp + $udp | Where-Object { $_.LocalAddress -notin @('127.0.0.1', '::1') }).Count -gt 0) { throw 'The isolated host exposed a non-loopback listener.' }
            if (!$result.headless.healthPassed) {
                try {
                    $health = Invoke-RestMethod -Uri "http://127.0.0.1:$ApiPort/api/health" -TimeoutSec 2
                    $app = Invoke-RestMethod -Uri "http://127.0.0.1:$ApiPort/api/health/app-info" -TimeoutSec 2
                    $listener = @(Get-NetTCPConnection -LocalPort $ApiPort -State Listen -ErrorAction Stop)
                    if (!$health.ok -or $health.service -cne 'intellitools-cliphost-tauri' -or $health.apiPort -ne $ApiPort -or $app.clipHost.runtime -cne 'gpui+rust' -or
                        $app.clipHost.fileVersion -cne $ExpectedVersion -or $app.clipHost.isDebugConfiguration -ne $false -or
                        $listener.Count -ne 1 -or $listener[0].OwningProcess -ne $hostProcess.Id -or $listener[0].LocalAddress -cne '127.0.0.1') {
                        throw 'Health runtime, version or listening process identity mismatch.'
                    }
                    $running = Get-Process -Id $hostProcess.Id -ErrorAction Stop
                    if ($running.Path -ine $exe -or (Get-FileHash -LiteralPath $running.Path).Hash.ToLowerInvariant() -cne $exeHash) { throw 'Running EXE identity mismatch.' }
                    $dllNames = @($info.files.name | Where-Object { $_ -like '*.dll' })
                    $result.headless.runtimeModules = @($running.Modules | Where-Object { $_.ModuleName -in $dllNames } | ForEach-Object {
                        [ordered]@{ name=$_.ModuleName; path=$_.FileName; fromOriginalPackage=([IO.Path]::GetDirectoryName($_.FileName) -ieq $package) }
                    })
                    if (!($result.headless.runtimeModules | Where-Object name -IEQ 'vcruntime140.dll') -or
                        @($result.headless.runtimeModules | Where-Object { !$_.fromOriginalPackage }).Count -gt 0) { throw 'VC runtime was not proven to load from the extracted package.' }
                    $health | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'health.json') -Encoding utf8NoBOM
                    $app | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'app-info.json') -Encoding utf8NoBOM
                    $result.headless.healthPassed = $true
                } catch { $result.headless.lastHealthError = $_.Exception.Message }
            }
            Start-Sleep -Milliseconds 250
            $hostProcess.Refresh()
        }
        if (!$hostProcess.HasExited) { throw 'The isolated host did not self-exit within 45 seconds.' }
        $hostProcess.WaitForExit()
        $result.headless.exitCode = $hostProcess.ExitCode
        $result.headless.normalSelfExit = $hostProcess.ExitCode -eq 0
        if (!$result.headless.normalSelfExit -or !$result.headless.healthPassed) { throw 'The isolated host did not prove healthy startup and normal self-exit.' }
    } catch { $result.headless.error = $_.Exception.ToString() }
    finally { if ($null -ne $hostProcess -and !$hostProcess.HasExited) { $hostProcess.Kill(); [void]$hostProcess.WaitForExit(5000) } }
}
$result | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'runtime-smoke.json') -Encoding utf8NoBOM
$result | ConvertTo-Json -Depth 12
if (!$result.loader.passed -or !$result.headless.healthPassed -or !$result.headless.normalSelfExit) {
    throw 'Portable runtime smoke failed; see the retained receipt and stderr.'
}
