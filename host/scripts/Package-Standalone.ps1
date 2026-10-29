[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$BuildLog,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [ValidateSet('Gpui', 'Tauri')][string]$Renderer = 'Gpui',
    [ValidateSet('', 'x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')][string]$Target = '',
    [switch]$IncludeNativeSymbols
)

$ErrorActionPreference = 'Stop'
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$zip = $OutputDirectory.TrimEnd('\', '/') + '.zip'
if ((Test-Path -LiteralPath $OutputDirectory) -or (Test-Path -LiteralPath $zip)) {
    throw 'Choose a new output directory and archive path; existing packages are not overwritten.'
}
$records = @(Get-Content -LiteralPath $BuildLog | ForEach-Object { $_ | ConvertFrom-Json })
if (!$records.Count -or $records[-1].reason -ne 'build-finished' -or $records[-1].success -ne $true) {
    throw 'The Cargo log must end with a successful build.'
}
$application = if ($Renderer -eq 'Gpui') { 'cliphost-gpui' } else { 'cliphost-app' }
$artifacts = @($records | Where-Object {
    $_.reason -eq 'compiler-artifact' -and $_.target.name -eq $application -and $_.executable
})
if ($artifacts.Count -ne 1) { throw "Expected one desktop executable; see $BuildLog" }
$artifact = $artifacts[0]
if ($artifact.features -contains 'native-acceptance') {
    throw 'Native acceptance builds are isolated test fixtures, not standalone application packages.'
}
if ($artifact.profile.opt_level -eq '0' -or $artifact.profile.test -or
    ($Renderer -eq 'Tauri' -and $artifact.features -notcontains 'custom-protocol')) {
    throw 'The executable must be an optimized standalone build for the selected renderer.'
}

$source = [IO.File]::OpenRead([IO.Path]::GetFullPath($artifact.executable))
$symbols = $null
$runtimeSources = [Collections.Generic.List[object]]::new()
$excludedRuntimeFiles = [Collections.Generic.List[object]]::new()
try {
    # Hold a read-only handle through validation and copying so another build
    # cannot replace the executable between these operations.
    . (Join-Path $PSScriptRoot 'Read-ExecutableBuildInfo.ps1')
    . (Join-Path $PSScriptRoot 'Assert-StandaloneBuildInfo.ps1')
    . (Join-Path $PSScriptRoot 'Read-PortableExecutable.ps1')
    . (Join-Path $PSScriptRoot 'Read-NativeSymbols.ps1')
    $image = Read-ClipHostPortableExecutable -Stream $source
    $architecture = $image.architecture
    # Portable self-updates become the installed manifest too. Keep Quicker's
    # package identity and independent compatibility floor in every new package.
    $packageCompatibility = [ordered]@{}
    if ($architecture -in @('x64', 'arm64')) {
        $compatibility = Get-Content -LiteralPath (Join-Path $PSScriptRoot '../launcher-compatibility.json') -Raw |
            ConvertFrom-Json -AsHashtable
        $baselineKey = if ($architecture -eq 'arm64') { 'arm64RequiredLauncherVersion' } else { 'requiredLauncherVersion' }
        if (!$compatibility.ContainsKey($baselineKey) -or
            [string]$compatibility[$baselineKey] -cnotmatch '^\d+\.\d+\.\d+\.\d+$') {
            throw "Invalid standalone launcher compatibility floor: $baselineKey"
        }
        $packageCompatibility.packageName = if ($architecture -eq 'arm64') { 'cea.cliphost.rust.arm64' } else { 'cea.cliphost.rust' }
        $packageCompatibility.launcherVersion = ([version][string]$compatibility[$baselineKey]).ToString(4)
    }
    if ($Target -and $Target -cne $image.target) {
        throw "The executable target $($image.target) does not match requested target $Target."
    }
    $executableBuild = Read-ClipHostExecutableBuildInfo -Path $artifact.executable
    Assert-ClipHostStandaloneBuildInfo -BuildInfo $executableBuild -Renderer $Renderer -OptLevel ([string]$artifact.profile.opt_level)
    if ($executableBuild.Contains('target') -and $executableBuild.target -cne $image.target) {
        throw "The embedded executable target $($executableBuild.target) does not match PE target $($image.target)."
    }
    # User packages, including feedback/diagnostic packages, do not need PDBs.
    # Explicit developer packages must contain symbols matching this exact EXE.
    if ($IncludeNativeSymbols) {
        $codeView = Read-ClipHostCodeViewIdentity -Stream $source
        if (!$codeView) { throw 'The executable has no native PDB identity.' }
        $pdbPath = Join-Path ([IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($artifact.executable))) $codeView.Name
        if (!(Test-Path -LiteralPath $pdbPath)) { throw "Requested native PDB was not found: $pdbPath" }
        $symbols = [IO.File]::OpenRead($pdbPath)
        Assert-ClipHostNativePdb -CodeView $codeView -Stream $symbols -Name ([IO.Path]::GetFileName($pdbPath))
    }
    # Use the redistributable CRT, never a developer/debug DLL or System32 copy.
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
    if (!(Test-Path -LiteralPath $vswhere)) { throw 'Visual Studio Installer is needed to locate the redistributable VC runtime.' }
    # An ARM64-only VS installation need not contain the x86/x64 tools component.
    # Search installed VS redistributables directly, never Windows system DLLs.
    $installations = @(& $vswhere -products '*' -property installationPath)
    if ($LASTEXITCODE -ne 0) { throw 'Could not locate the installed Visual C++ tools.' }
    $candidates = @(foreach ($installation in $installations) {
        Get-ChildItem -Path (Join-Path $installation "VC/Redist/MSVC/*/$architecture/Microsoft.VC*.CRT/vcruntime140.dll") -File -ErrorAction SilentlyContinue
    })
    $runtime = $candidates | Sort-Object { [version]$_.VersionInfo.FileVersionRaw } -Descending | Select-Object -First 1
    if (!$runtime) { throw "Redistributable VC runtime for $architecture was not found." }
    $runtimeFiles = @(Get-ChildItem -LiteralPath $runtime.DirectoryName -Filter '*.dll' -File)
    foreach ($file in $runtimeFiles) {
        $runtimeSource = [IO.File]::OpenRead($file.FullName)
        $runtimeSources.Add([pscustomobject]@{ file=$file; stream=$runtimeSource })
        $runtimeImage = Read-ClipHostPortableExecutable -Stream $runtimeSource
        if ($architecture -ceq 'arm64' -and $file.Name -ieq 'vcruntime140_1.dll' -and $runtimeImage.architecture -ceq 'x64') {
            # Microsoft's ARM64 redist also carries the x64 FH4 runtime for
            # emulated/ARM64EC programs. It is not a native ARM64 dependency.
            # https://devblogs.microsoft.com/cppblog/making-cpp-exception-handling-smaller-x64/
            $runtimeSources.RemoveAt($runtimeSources.Count - 1)
            $runtimeSource.Dispose()
            $excludedRuntimeFiles.Add([ordered]@{name=$file.Name; architecture='x64'; reason='x64 FH4 runtime is not part of the native ARM64 payload'})
            continue
        }
        if ($runtimeImage.architecture -cne $architecture) {
            throw "VC runtime $($file.Name) architecture $($runtimeImage.architecture) does not match executable architecture $architecture."
        }
    }
    New-Item -ItemType Directory -Path $OutputDirectory | Out-Null
    $exe = Join-Path $OutputDirectory 'cliphost-app.exe'
    $source.Position = 0
    $destination = [IO.File]::Open($exe, [IO.FileMode]::CreateNew)
    try { $source.CopyTo($destination) } finally { $destination.Dispose() }
    if ($symbols) {
        # Preserve the RSDS basename even though the portable EXE is renamed.
        $symbols.Position = 0
        $destination = [IO.File]::Open((Join-Path $OutputDirectory $codeView.Name), [IO.FileMode]::CreateNew)
        try { $symbols.CopyTo($destination) } finally { $destination.Dispose() }
    }
    foreach ($runtimeSource in $runtimeSources) {
        $destination = [IO.File]::Open((Join-Path $OutputDirectory $runtimeSource.file.Name), [IO.FileMode]::CreateNew)
        try { $runtimeSource.stream.CopyTo($destination) } finally { $destination.Dispose() }
    }
} finally {
    if ($symbols) { $symbols.Dispose() }
    foreach ($runtimeSource in $runtimeSources) { $runtimeSource.stream.Dispose() }
    $source.Dispose()
}
# Keep the recovery entry beside the portable EXE.
# These files participate in the same payload size/hash checks as the binaries.
foreach ($name in @('Start-ClipHostDiagnostics.ps1', 'Start-ClipHostDiagnostics.cmd')) {
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination (Join-Path $OutputDirectory $name)
}
$files = @(Get-ChildItem -LiteralPath $OutputDirectory -File | Sort-Object Name | ForEach-Object {
    [ordered]@{ name=$_.Name; sha256=(Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant(); sizeBytes=$_.Length }
})
$files | ForEach-Object { "$($_.sha256)  $($_.name)" } |
    Set-Content (Join-Path $OutputDirectory 'SHA256SUMS.txt') -Encoding utf8NoBOM
$hash = ($files | Where-Object name -eq 'cliphost-app.exe').sha256
$standaloneInfo = [ordered]@{
    product='ClipHost Rust'; builtAtUtc=[datetime]::UtcNow.ToString('o')
    executable='cliphost-app.exe'; sha256=$hash; architecture=$architecture; target=$image.target
    version=([string]$executableBuild.version + '.0')
    renderer=$Renderer.ToLowerInvariant(); sourceExecutable=($application + '.exe')
    profile='release'; embeddedFrontend=($Renderer -eq 'Tauri'); files=$files
    executableBuild=$executableBuild
    vcRuntime=$runtime.VersionInfo.FileVersionRaw.ToString()
    vcRuntimeExcluded=$excludedRuntimeFiles.ToArray()
    rustc=(& rustc --version); cargo=(& cargo --version)
}
foreach ($key in $packageCompatibility.Keys) { $standaloneInfo[$key] = $packageCompatibility[$key] }
$standaloneInfo | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $OutputDirectory 'build-info.json') -Encoding utf8NoBOM
$rendererRequirements = if ($Renderer -eq 'Gpui') {
    'Native GPUI rendering; WebView2, frontend assets and a frontend server are not required.'
} else {
    'Microsoft Edge WebView2 Runtime must be installed on the target computer.'
}
$migrationInstructions = if ($Renderer -eq 'Tauri') {
@'
Optional C# / upgraded WPF data import to a separate folder (no merge):
cliphost-app.exe --import-csharp-data SOURCE --import-destination DEST --import-report REPORT [--dry-run]
SOURCE contains userdata.db, Image and Configs. Close the old program normally before the final import.
DEST must be absent or empty. REPORT must be a new file outside SOURCE and DEST; its parent must exist.
The command exits without starting windows or networking. Use --dry-run first and inspect REPORT.
History, favorites, owned images and four compatible core configs are imported.
Original documents/configs are archived under DEST/migration. Unsupported local image references fail explicitly.
After success, choose DEST in the existing data-folder setting and restart to use it.
'@
} else { '' }
@"
ClipHost Rust — standalone Windows build

Architecture: $architecture ($($image.target)). Use the package matching your Windows device architecture.
Extract the entire folder and run cliphost-app.exe. Keep the included VC runtime DLLs beside it.
Node.js, Rust and a running Vite server are not required.
$rendererRequirements

Data: reuse the saved Rust location. On first update, detect and remember an existing C# / WPF data folder,
including Documents\Quicker\Ceastld; otherwise use %LocalAppData%\BetterClipTauri.
Existing Rust data is retained. Manage > Data can switch to the previous default data folder.
Local API: http://127.0.0.1:28965
Optional overrides: BETTERCLIP_TAURI_DATA_ROOT, INTELLITOOLS_CLIPHOST_TAURI_PORT.

This package does not install itself or replace the existing C# / Quicker launch entry.
Only one Rust instance can run at a time. Exit an existing Rust instance before switching builds.
Quicker integration is optional; cloud synchronization is currently disabled.

Normal startup errors and ordinary Rust panics show a native error dialog.
Press Ctrl+C in the dialog to copy its details. The full available stack is in the startup log
under %LocalAppData%\ClipHostRust\diagnostics. PDB files are not required for feedback:
send the error and logs; developers can analyze addresses using matching archived symbols.
For an additional feedback ZIP, double-click Start-ClipHostDiagnostics.cmd.
It records the exit code, startup errors and related Windows crash events.
The ZIP is saved under %LocalAppData%\ClipHostRust\diagnostics; send it with your report.
Clipboard contents, databases and account credentials are not collected or uploaded.
An existing ClipHost process is left running; diagnostics do not stop or restart it.

$migrationInstructions
"@ | Set-Content (Join-Path $OutputDirectory 'README.txt') -Encoding utf8NoBOM
. (Join-Path $PSScriptRoot 'New-ZipArchive.ps1')
New-ClipHostZipArchive -SourceDirectory $OutputDirectory -ArchivePath $zip -RootDirectory 'ClipHost-Rust'
[ordered]@{ executable=$exe; archive=$zip; sha256=$hash; vcRuntime=$runtime.VersionInfo.FileVersionRaw.ToString(); buildLogs=(Split-Path ([IO.Path]::GetFullPath($BuildLog))) } | ConvertTo-Json
