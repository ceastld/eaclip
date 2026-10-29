function New-ClipHostZipArchive {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$SourceDirectory,
        [Parameter(Mandatory)][string]$ArchivePath,
        [string]$RootDirectory = ''
    )

    $sourceRoot = [IO.Path]::GetFullPath($SourceDirectory)
    $archiveFullPath = [IO.Path]::GetFullPath($ArchivePath)
    if (!(Test-Path -LiteralPath $sourceRoot -PathType Container)) {
        throw "Archive source directory does not exist: $sourceRoot"
    }
    if ($RootDirectory -and ($RootDirectory -in @('.', '..') -or $RootDirectory.Contains('/') -or
        $RootDirectory.Contains('\'))) {
        throw 'Archive RootDirectory must be a single folder name.'
    }
    $sourcePrefix = $sourceRoot.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    if ($archiveFullPath.StartsWith($sourcePrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'The archive must be outside its source directory.'
    }
    if ([Enum]::GetNames([IO.Compression.CompressionLevel]) -notcontains 'SmallestSize') {
        throw 'ZIP packaging requires PowerShell 7 on a modern .NET runtime with SmallestSize compression.'
    }

    $prefix = if ($RootDirectory) { "$RootDirectory/" } else { '' }
    # CreateNew also prevents a concurrent packager from replacing an existing package.
    $stream = [IO.File]::Open($archiveFullPath, [IO.FileMode]::CreateNew,
        [IO.FileAccess]::Write, [IO.FileShare]::None)
    $complete = $false
    try {
        $archive = [IO.Compression.ZipArchive]::new($stream,
            [IO.Compression.ZipArchiveMode]::Create, $true, [Text.Encoding]::UTF8)
        try {
            if ($prefix) { [void]$archive.CreateEntry($prefix) }
            foreach ($item in Get-ChildItem -LiteralPath $sourceRoot -Recurse -Force | Sort-Object FullName) {
                $relative = [IO.Path]::GetRelativePath($sourceRoot, $item.FullName).Replace('\', '/')
                if ($item.PSIsContainer) {
                    [void]$archive.CreateEntry("$prefix$relative/")
                } else {
                    [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive,
                        $item.FullName, "$prefix$relative", [IO.Compression.CompressionLevel]::SmallestSize)
                }
            }
        } finally { $archive.Dispose() }
        $complete = $true
    } finally {
        $stream.Dispose()
        if (!$complete) { Remove-Item -LiteralPath $archiveFullPath -ErrorAction SilentlyContinue }
    }
}
