# 只检查 Cargo 回执和 PE 字节，不启动 CLI、宿主或其他程序。
function Get-ClipHostStandaloneCliArtifact {
    param([object[]]$Records, [string]$ApplicationExecutable)
    $artifacts = @($Records | Where-Object {
        $_.reason -eq 'compiler-artifact' -and $_.target.name -ceq 'eaclip-cli' -and
        $_.target.kind -contains 'bin' -and $_.executable
    })
    if ($artifacts.Count -ne 1) { throw '需要同一次 Cargo 构建中唯一的 eaclip-cli 可执行回执。' }
    $artifact = $artifacts[0]
    if ($artifact.profile.opt_level -eq '0' -or $artifact.profile.test -or $artifact.features -contains 'native-acceptance') {
        throw 'eaclip-cli 必须为优化后的非测试构建。'
    }
    $path = [IO.Path]::GetFullPath($artifact.executable)
    if ([IO.Path]::GetFileName($path) -cne 'eaclip-cli.exe' -or
        [IO.Path]::GetDirectoryName($path) -ine [IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($ApplicationExecutable))) {
        throw 'eaclip-cli 必须与同次构建的主程序位于同一目录。'
    }
    return $artifact
}

function Open-ClipHostStandaloneCliPayload {
    param([string]$CliExecutable, [string]$ApplicationExecutable, [string]$Architecture)
    $path = [IO.Path]::GetFullPath($CliExecutable)
    $localSpelling = $path -replace '^\\\\\?\\', ''
    if ($localSpelling -notmatch '^[A-Za-z]:[\\/]' -or
        @($localSpelling -split '[\\/]' | Where-Object { $_ -match '^(?i:OneDrive|iCloud)' -or $_ -ieq 'Dropbox' }).Count) {
        throw 'CLI 载荷必须位于普通本地盘路径，不能使用 UNC 或云盘。'
    }
    if ([IO.Path]::GetFileName($path) -cne 'eaclip-cli.exe' -or
        [IO.Path]::GetDirectoryName($path) -ine [IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($ApplicationExecutable))) {
        throw 'CLI 载荷必须是主程序同目录的 eaclip-cli.exe。'
    }
    # 在打开前逐级检查，拒绝链接、占位和云盘目录。
    $cursor = $path
    while ($cursor) {
        $item = Get-Item -LiteralPath $cursor -Force -ErrorAction Stop
        if (([int]$item.Attributes -band 0x441400) -ne 0 -or
            $item.Name -match '^(?i:OneDrive|iCloud)' -or $item.Name -ieq 'Dropbox') {
            throw 'CLI 载荷路径包含链接、占位或云盘目录。'
        }
        $parent = [IO.Directory]::GetParent($cursor)
        $cursor = if ($parent) { $parent.FullName } else { $null }
    }
    $stream = [IO.File]::OpenRead($path)
    try {
        $image = Read-ClipHostPortableExecutable -Stream $stream
        if ($image.architecture -cne $Architecture) { throw 'CLI 与主程序的 PE 架构不匹配。' }
        return [pscustomobject]@{ path=$path; stream=$stream; image=$image }
    } catch { $stream.Dispose(); throw }
}
