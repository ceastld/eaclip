[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$ExecutablePath,
    [Parameter(Mandatory)][string]$PdbDirectory,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [Parameter(Mandatory)][ValidatePattern('^\d+\.\d+\.\d+$')][string]$ExpectedVersion,
    [Parameter(Mandatory)][ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')][string]$ExpectedTarget,
    [Parameter(Mandatory)][ValidatePattern('^[0-9a-fA-F]{40}$')][string]$SourceCommit
)

# CI-only symbol retention, separate from portable/release assets. Missing
# symbols are explicitly recorded; a present but mismatched PDB is never saved.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'Read-PortableExecutable.ps1')
. (Join-Path $PSScriptRoot 'Read-ExecutableBuildInfo.ps1')
. (Join-Path $PSScriptRoot 'Assert-StandaloneBuildInfo.ps1')
. (Join-Path $PSScriptRoot 'Read-NativeSymbols.ps1')
$ExecutablePath = [IO.Path]::GetFullPath($ExecutablePath)
$PdbDirectory = [IO.Path]::GetFullPath($PdbDirectory)
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $OutputDirectory) { throw 'Internal symbols output already exists; preserve the original archive.' }
$source = [IO.File]::OpenRead($ExecutablePath)
$symbols = $null
try {
    $pe = Read-ClipHostPortableExecutable -Stream $source
    $build = Read-ClipHostExecutableBuildInfo -Path $ExecutablePath
    Assert-ClipHostStandaloneBuildInfo -BuildInfo $build -Renderer Gpui
    if ($pe.target -cne $ExpectedTarget -or $build.target -cne $ExpectedTarget -or $build.version -cne $ExpectedVersion) {
        throw 'Internal symbols require the exact expected executable version and target.'
    }
    $source.Position = 0
    $exeHash = (Get-FileHash -InputStream $source -Algorithm SHA256).Hash.ToLowerInvariant()
    $codeView = Read-ClipHostCodeViewIdentity -Stream $source
    $record = [ordered]@{
        schemaVersion=1; version=$build.version; target=$pe.target; architecture=$pe.architecture
        sourceCommit=$SourceCommit.ToLowerInvariant(); executable=[IO.Path]::GetFileName($ExecutablePath); executableSha256=$exeHash
        status='unavailable'; reason='Executable has no RSDS identity.'
        pdbName=$null; pdbGuid=$null; pdbAge=$null; pdbSha256=$null; pdbSizeBytes=$null
    }
    if ($codeView) {
        $record.pdbName = $codeView.Name
        $record.pdbGuid = $codeView.Guid.ToString()
        $record.pdbAge = $codeView.Age
        $record.reason = 'Matching PDB was not produced or retained in the build directory.'
        $pdb = Join-Path $PdbDirectory $codeView.Name
        if (Test-Path -LiteralPath $pdb) {
            $symbols = [IO.File]::OpenRead($pdb)
            Assert-ClipHostNativePdb -CodeView $codeView -Stream $symbols -Name ([IO.Path]::GetFileName($pdb))
            $symbols.Position = 0
            $record.pdbSha256 = (Get-FileHash -InputStream $symbols -Algorithm SHA256).Hash.ToLowerInvariant()
            $record.pdbSizeBytes = $symbols.Length
            $record.status = 'retained'
            $record.reason = $null
        }
    }
    New-Item -ItemType Directory -Path $OutputDirectory | Out-Null
    if ($symbols) {
        $symbols.Position = 0
        $destination = [IO.File]::Open((Join-Path $OutputDirectory $codeView.Name), [IO.FileMode]::CreateNew)
        try { $symbols.CopyTo($destination) } finally { $destination.Dispose() }
    } else { Write-Warning $record.reason }
    $record | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'symbols.json') -Encoding utf8NoBOM
    $record | ConvertTo-Json -Depth 5
} finally {
    if ($symbols) { $symbols.Dispose() }
    $source.Dispose()
}
