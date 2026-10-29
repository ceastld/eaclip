[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$ExecutablePath,
    [ValidateSet('Release', 'Debug')][string]$Configuration = 'Release'
)

# Explicit identity gate for newly built GPUI executables. Historical packages
# predate VERSIONINFO and remain readable by the existing package validators.
# Reads PE resources only; never builds, launches, modifies or uploads the EXE.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (!$IsWindows) { throw 'Validate executable resources with PowerShell 7 on Windows.' }
. (Join-Path $PSScriptRoot 'Read-ExecutableBuildInfo.ps1')
. (Join-Path $PSScriptRoot 'Assert-StandaloneBuildInfo.ps1')

$ExecutablePath = [IO.Path]::GetFullPath($ExecutablePath)
$item = Get-Item -LiteralPath $ExecutablePath -Force
if ($item.PSIsContainer -or ($item.Attributes -band (0x1000 -bor 0x40000 -bor 0x400000))) {
    throw 'Choose an existing local executable, not a directory or cloud placeholder.'
}
# Keep both resource readers bound to the same immutable file during validation.
$stream = [IO.File]::Open($ExecutablePath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
try {
    $build = Read-ClipHostExecutableBuildInfo -Path $ExecutablePath
    if ($build['renderer'] -cne 'gpui' -or $build['embedded_frontend'] -cne 'false' -or
        $build['custom_protocol'] -cne 'false' -or $build['native_acceptance'] -cne 'false') {
        throw 'Expected the native GPUI application, not a Tauri or acceptance fixture executable.'
    }
    if ($Configuration -eq 'Release') {
        Assert-ClipHostStandaloneBuildInfo -BuildInfo $build -Renderer Gpui
    }
    $cargoVersion = [string]$build['version']
    $expectedVersion = $null
    if ($cargoVersion -cnotmatch '^\d+\.\d+\.\d+$' -or
        ![version]::TryParse(($cargoVersion + '.0'), [ref]$expectedVersion)) {
        throw 'The embedded Cargo version must contain three stable numeric components.'
    }
    $expectedText = $expectedVersion.ToString(4)
    $file = [Diagnostics.FileVersionInfo]::GetVersionInfo($ExecutablePath)
    foreach ($field in @('FileVersion', 'ProductVersion')) {
        if ($file.$field -cne $expectedText) {
            throw "Windows VERSIONINFO $field must match embedded Cargo version $expectedText; found '$($file.$field)'."
        }
    }
    $numericFile = [version]::new($file.FileMajorPart, $file.FileMinorPart, $file.FileBuildPart, $file.FilePrivatePart)
    $numericProduct = [version]::new($file.ProductMajorPart, $file.ProductMinorPart, $file.ProductBuildPart, $file.ProductPrivatePart)
    if ($numericFile -ne $expectedVersion -or $numericProduct -ne $expectedVersion) {
        throw 'The numeric Windows file/product version does not match the embedded Cargo version.'
    }
    $identity = [ordered]@{
        ProductName = 'ClipHost'; FileDescription = 'ClipHost'
        InternalName = 'cliphost-gpui'; OriginalFilename = 'cliphost-app.exe'
    }
    foreach ($field in $identity.Keys) {
        if ($file.$field -cne $identity[$field]) {
            throw "Windows VERSIONINFO $field must be '$($identity[$field])'; found '$($file.$field)'."
        }
    }
    if ($file.IsDebug -ne ($Configuration -eq 'Debug')) {
        throw "The Windows VS_FF_DEBUG flag does not match $Configuration."
    }
    [ordered]@{
        status = 'verified'; executable = $ExecutablePath; renderer = 'gpui'
        configuration = $Configuration; cargoVersion = $cargoVersion
        fileVersion = $file.FileVersion; productVersion = $file.ProductVersion
        productName = $file.ProductName; fileDescription = $file.FileDescription
        internalName = $file.InternalName; originalFilename = $file.OriginalFilename
        isDebug = $file.IsDebug
    } | ConvertTo-Json
} finally {
    $stream.Dispose()
}
