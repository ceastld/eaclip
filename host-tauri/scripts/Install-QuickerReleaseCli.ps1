[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$OutputDirectory,
    [switch]$Login
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# The release's assembly version predates its tag. Keep both identities explicit.
$releaseTag = 'v0.1.1'
$expectedSha256 = '9281585532e12cebea60792bf1dd6c3abfb987a86cc9ad66e86ab65eab2b7356'
$expectedAssemblyVersion = '0.1.0.0'
$assetName = 'quicker-win-x64.zip'

function Invoke-QuickerInstallerProcess {
    param(
        [string]$FileName,
        [string[]]$Arguments,
        [hashtable]$Environment = @{},
        [AllowNull()][string]$StandardInput = $null,
        [string]$FailureMessage
    )
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FileName
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.RedirectStandardInput = $true
    foreach ($argument in $Arguments) { $startInfo.ArgumentList.Add($argument) }
    # Each secret is supplied only to the child that needs it, never inherited by
    # the CLI or printed with the captured output.
    foreach ($name in @('QUICKER_CLI_READ_TOKEN', 'QUICKER_USERNAME', 'QUICKER_PASSWORD')) {
        [void]$startInfo.Environment.Remove($name)
    }
    foreach ($name in $Environment.Keys) {
        if ($null -eq $Environment[$name]) { [void]$startInfo.Environment.Remove($name) }
        else { $startInfo.Environment[$name] = [string]$Environment[$name] }
    }
    $child = [Diagnostics.Process]::new()
    $child.StartInfo = $startInfo
    try {
        if (!$child.Start()) { throw $FailureMessage }
        $stdout = $child.StandardOutput.ReadToEndAsync()
        $stderr = $child.StandardError.ReadToEndAsync()
        if ($null -ne $StandardInput) { $child.StandardInput.WriteLine($StandardInput) }
        $child.StandardInput.Close()
        if (!$child.WaitForExit(60000)) {
            $child.Kill($true)
            $child.WaitForExit()
            throw $FailureMessage
        }
        $capturedOutput = $stdout.GetAwaiter().GetResult()
        [void]$stderr.GetAwaiter().GetResult()
        if ($child.ExitCode -ne 0) { throw $FailureMessage }
        return $capturedOutput
    } catch {
        # Child output may contain account details or a server response. Do not
        # turn it into an Actions annotation, exception, or installation result.
        throw $FailureMessage
    } finally { $child.Dispose() }
}

$onGitHubActions = $env:GITHUB_ACTIONS -ceq 'true'
if ($Login) {
    if (!$onGitHubActions -or [string]::IsNullOrWhiteSpace($env:RUNNER_TEMP) -or
        !(Test-Path -LiteralPath $env:RUNNER_TEMP -PathType Container) -or
        $env:RUNNER_ENVIRONMENT -cne 'github-hosted') {
        throw 'Quicker login is allowed only on an ephemeral GitHub-hosted Actions runner.'
    }
    if ([string]::IsNullOrWhiteSpace($env:QUICKER_USERNAME) -or
        [string]::IsNullOrWhiteSpace($env:QUICKER_PASSWORD)) {
        throw 'QUICKER_USERNAME and QUICKER_PASSWORD are required for Quicker login.'
    }
}
if ($onGitHubActions -and [string]::IsNullOrWhiteSpace($env:QUICKER_CLI_READ_TOKEN)) {
    throw 'QUICKER_CLI_READ_TOKEN with read access to QuickerHub/quicker is required.'
}
$destination = [IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $destination) { throw 'Quicker CLI output directory must not already exist.' }
$gh = (Get-Command gh -CommandType Application -ErrorAction Stop).Source
New-Item -ItemType Directory -Path $destination | Out-Null
$ghEnvironment = @{}
if (![string]::IsNullOrWhiteSpace($env:QUICKER_CLI_READ_TOKEN)) {
    $ghEnvironment.GH_TOKEN = $env:QUICKER_CLI_READ_TOKEN
    $ghEnvironment.GITHUB_TOKEN = $null
}
$null = Invoke-QuickerInstallerProcess -FileName $gh -Arguments @(
    'release', 'download', $releaseTag, '--repo', 'QuickerHub/quicker',
    '--pattern', $assetName, '--dir', $destination
) -Environment $ghEnvironment -FailureMessage 'Could not download the pinned Quicker CLI release.'
$archive = Join-Path $destination $assetName
if (!(Test-Path -LiteralPath $archive -PathType Leaf) -or
    (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -cne $expectedSha256) {
    throw 'Pinned Quicker CLI archive SHA-256 mismatch.'
}
Expand-Archive -LiteralPath $archive -DestinationPath $destination
$executable = Join-Path $destination 'win-x64/quicker.exe'
$assembly = Join-Path $destination 'win-x64/quicker.dll'
foreach ($path in @($executable, $assembly,
    (Join-Path $destination 'win-x64/quicker.deps.json'),
    (Join-Path $destination 'win-x64/quicker.runtimeconfig.json'))) {
    if (!(Test-Path -LiteralPath $path -PathType Leaf)) { throw 'Pinned Quicker CLI bundle is incomplete.' }
}
$identity = [Reflection.AssemblyName]::GetAssemblyName($assembly)
if ($identity.Name -cne 'quicker' -or $identity.Version.ToString() -cne $expectedAssemblyVersion) {
    throw 'Pinned Quicker CLI assembly identity mismatch.'
}
$cliEnvironment = @{ GH_TOKEN=$null; GITHUB_TOKEN=$null }
$help = Invoke-QuickerInstallerProcess -FileName $executable -Arguments @('--help') `
    -Environment $cliEnvironment -FailureMessage 'Could not run the pinned Quicker CLI; install the .NET 8 runtime.'
if ($help -notmatch 'quicker dep upload' -or $help -notmatch 'quicker dep find' -or
    $help -notmatch '--password-stdin') {
    throw 'Pinned Quicker CLI does not provide the required release commands.'
}
if ($Login) {
    # The disposable runner owns this AppData session. The workflow must run
    # `quicker logout` in its always-cleanup step, including after failed uploads.
    $null = Invoke-QuickerInstallerProcess -FileName $executable -Arguments @(
        'login', '--username', $env:QUICKER_USERNAME, '--password-stdin'
    ) -Environment $cliEnvironment -StandardInput $env:QUICKER_PASSWORD `
        -FailureMessage 'Quicker login failed; verify the configured Actions account credentials.'
}
[ordered]@{ executable=$executable; version='0.1.0'; release=$releaseTag; sha256=$expectedSha256 } |
    ConvertTo-Json -Compress
