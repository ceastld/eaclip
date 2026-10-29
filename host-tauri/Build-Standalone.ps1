[CmdletBinding()]
param(
    [string]$OutputDirectory,
    [ValidateSet('Gpui', 'Tauri')][string]$Renderer = 'Gpui',
    [ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')][string]$Target,
    [switch]$IncludeNativeSymbols
)

# Build a separate Rust desktop package. Does not install, launch, stop a host,
# or touch the C# build/publish path or either backend's application data.
$ErrorActionPreference = 'Stop'
if (!$IsWindows) { throw 'Build the Windows desktop package with PowerShell 7 on Windows.' }
foreach ($tool in @('cargo', 'rustc')) { Get-Command $tool -ErrorAction Stop | Out-Null }
if ($Renderer -eq 'Tauri') { Get-Command npm -ErrorAction Stop | Out-Null }
# Always pass an explicit target, including on native ARM runners. Do not let a
# developer's Cargo configuration silently select a different package architecture.
if (!$Target) {
    $resolvedTarget = $env:CARGO_BUILD_TARGET
    if (!$resolvedTarget) {
        $compilerInfo = @(& rustc -vV)
        if ($LASTEXITCODE -ne 0) { throw 'Could not determine the Rust host target.' }
        $resolvedTarget = ($compilerInfo | Where-Object { $_ -cmatch '^host: ' }) -creplace '^host: ', ''
    }
    if (@('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc') -cnotcontains $resolvedTarget) {
        throw "Unsupported standalone target '$resolvedTarget'. Use x86_64-pc-windows-msvc or aarch64-pc-windows-msvc."
    }
    $Target = $resolvedTarget
}
$rustRoot = $PSScriptRoot
$frontend = [IO.Path]::GetFullPath((Join-Path $rustRoot '../frontend'))
$ClipRepoRoot = [IO.Path]::GetFullPath((Join-Path $rustRoot '..'))
$run = Join-Path $ClipRepoRoot ('.tmp/cliphost-standalone-build-' + [guid]::NewGuid().ToString('N'))
if (!$OutputDirectory) { $OutputDirectory = Join-Path $run 'ClipHost-Rust' }
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$zip = $OutputDirectory.TrimEnd('\', '/') + '.zip'
if (Test-Path -LiteralPath $OutputDirectory) { throw "Output already exists: $OutputDirectory. Choose a new directory." }
if (Test-Path -LiteralPath $zip) { throw "Archive already exists: $zip. Choose a new directory." }
if ($Renderer -eq 'Tauri' -and !(Test-Path -LiteralPath (Join-Path $frontend 'node_modules'))) {
    throw "Frontend dependencies are missing. Run npm ci in $frontend first."
}
New-Item -ItemType Directory -Path $run | Out-Null

if ($Renderer -eq 'Tauri') {
    Push-Location $frontend
    try {
        Write-Host 'Building the embedded frontend...'
        npm run build *> (Join-Path $run 'frontend.log')
        if ($LASTEXITCODE -ne 0) { throw "Frontend build failed. See $run/frontend.log" }
    } finally { Pop-Location }
}

Push-Location $rustRoot
try {
    Write-Host "Building the $Renderer Windows Release executable for $Target..."
    $buildLog = Join-Path $run 'cargo.jsonl'
    $application = if ($Renderer -eq 'Gpui') { 'cliphost-gpui' } else { 'cliphost-app' }
    $buildArgs = @('build', '--locked', '--release', '--package', $application, '--bin', $application, '--target', $Target)
    if ($Renderer -eq 'Tauri') { $buildArgs += @('--features', 'custom-protocol') }
    $buildArgs += '--message-format=json-render-diagnostics'
    & cargo @buildArgs `
        1> $buildLog 2> (Join-Path $run 'cargo.stderr.log')
    if ($LASTEXITCODE -ne 0) { throw "Rust build failed. See $run/cargo.stderr.log" }
    & (Join-Path $rustRoot 'scripts/Package-Standalone.ps1') -BuildLog $buildLog -OutputDirectory $OutputDirectory -Renderer $Renderer -Target $Target -IncludeNativeSymbols:$IncludeNativeSymbols
} finally { Pop-Location }
