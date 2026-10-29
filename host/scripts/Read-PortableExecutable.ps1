# Inspect Windows images as data, including images for a different CPU. No image
# or imports are loaded. A supplied stream remains open and keeps its position.
function Read-ClipHostPortableExecutable {
    [CmdletBinding(DefaultParameterSetName = 'Path')]
    param(
        [Parameter(Mandatory, ParameterSetName = 'Path')][string]$Path,
        [Parameter(Mandatory, ParameterSetName = 'Stream')][IO.Stream]$Stream
    )

    $ownsStream = $PSCmdlet.ParameterSetName -eq 'Path'
    if ($ownsStream) { $Stream = [IO.File]::OpenRead([IO.Path]::GetFullPath($Path)) }
    if (!$Stream.CanRead -or !$Stream.CanSeek) {
        if ($ownsStream) { $Stream.Dispose() }
        throw 'A readable, seekable stream is required to inspect a Windows PE image.'
    }
    $position = $Stream.Position
    $reader = [IO.BinaryReader]::new($Stream, [Text.Encoding]::UTF8, $true)
    try {
        if ($Stream.Length -lt 0x40) { throw 'Invalid or truncated Windows PE DOS header.' }
        $Stream.Position = 0
        if ($reader.ReadUInt16() -ne 0x5a4d) { throw 'Invalid Windows PE DOS signature.' }
        $Stream.Position = 0x3c
        $header = $reader.ReadInt32()
        if ($header -lt 0x40 -or ([long]$header + 24) -gt $Stream.Length) {
            throw 'Invalid or truncated Windows PE header offset.'
        }
        $Stream.Position = $header
        if ($reader.ReadUInt32() -ne 0x00004550) { throw 'Invalid Windows PE signature.' }
        $machine = $reader.ReadUInt16()
        $sections = $reader.ReadUInt16()
        $Stream.Position = $header + 20
        $optionalSize = $reader.ReadUInt16()
        $optionalOffset = [long]$header + 24
        if ($optionalSize -lt 2 -or ($optionalOffset + $optionalSize + [long]$sections * 40) -gt $Stream.Length) {
            throw 'Invalid or truncated Windows PE optional header or section table.'
        }
        $Stream.Position = $optionalOffset
        $magic = $reader.ReadUInt16()
        $architecture, $target, $expectedMagic, $minimumSize = switch ($machine) {
            0x8664 { 'x64', 'x86_64-pc-windows-msvc', 0x20b, 112 }
            0x014c { 'x86', 'i686-pc-windows-msvc', 0x10b, 96 }
            0xaa64 { 'arm64', 'aarch64-pc-windows-msvc', 0x20b, 112 }
            default { throw ('Unsupported Windows PE machine: 0x{0:x4}.' -f $machine) }
        }
        if ($magic -ne $expectedMagic -or $optionalSize -lt $minimumSize) {
            throw "Invalid Windows PE optional header for $architecture."
        }
        [pscustomobject]@{ architecture=$architecture; target=$target; machine=$machine }
    } finally {
        $reader.Dispose()
        $Stream.Position = $position
        if ($ownsStream) { $Stream.Dispose() }
    }
}
