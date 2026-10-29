# Read identities as data only. No image loading, DbgHelp, symbol servers or
# environment changes are needed to match a native MSVC PDB to its exact PE.
if (-not ('ClipHostPackaging.NativeSymbols' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Text;

namespace ClipHostPackaging {
    public sealed class SymbolIdentity {
        public string Name { get; set; }
        public Guid Guid { get; set; }
        public uint Age { get; set; }
    }

    public static class NativeSymbols {
        private static void Range(Stream s, long offset, long size) {
            if (offset < 0 || size < 0 || offset > s.Length || size > s.Length - offset)
                throw new InvalidDataException("Invalid or truncated native symbol metadata.");
        }
        private static byte[] Bytes(BinaryReader r, int count) {
            byte[] bytes = r.ReadBytes(count);
            if (bytes.Length != count) throw new InvalidDataException("Truncated native symbol metadata.");
            return bytes;
        }
        private static long Offset(BinaryReader r, long table, int sections, uint rva, uint size) {
            for (int i = 0; i < sections; i++) {
                r.BaseStream.Position = table + i * 40 + 12;
                uint address = r.ReadUInt32(), rawSize = r.ReadUInt32(), raw = r.ReadUInt32();
                if (rva >= address && (ulong)rva + size <= (ulong)address + rawSize) {
                    long offset = (long)raw + rva - address;
                    Range(r.BaseStream, offset, size);
                    return offset;
                }
            }
            throw new InvalidDataException("Native PE debug directory is outside a file-backed section.");
        }
        public static SymbolIdentity ReadImage(Stream stream) {
            long saved = stream.Position;
            try {
                using (var r = new BinaryReader(stream, Encoding.UTF8, true)) {
                    Range(stream, 0, 64); stream.Position = 0;
                    if (r.ReadUInt16() != 0x5a4d) throw new InvalidDataException("Invalid native PE DOS signature.");
                    stream.Position = 0x3c; long pe = r.ReadUInt32(); Range(stream, pe, 24);
                    stream.Position = pe;
                    if (r.ReadUInt32() != 0x4550) throw new InvalidDataException("Invalid native PE signature.");
                    r.ReadUInt16(); int sections = r.ReadUInt16();
                    stream.Position = pe + 20; int optionalSize = r.ReadUInt16();
                    long optional = pe + 24, table = optional + optionalSize;
                    Range(stream, optional, optionalSize); Range(stream, table, sections * 40L);
                    if (optionalSize < 2) throw new InvalidDataException("Invalid native PE optional header.");
                    stream.Position = optional; int magic = r.ReadUInt16();
                    int directories = magic == 0x20b ? 112 : magic == 0x10b ? 96 : 0;
                    if (directories == 0 || optionalSize < directories)
                        throw new InvalidDataException("Invalid native PE optional header.");
                    stream.Position = optional + directories - 4;
                    if (r.ReadUInt32() < 7) return null;
                    if (optionalSize < directories + 7 * 8)
                        throw new InvalidDataException("Truncated native PE debug directory entry.");
                    stream.Position = optional + directories + 6 * 8;
                    uint rva = r.ReadUInt32(), size = r.ReadUInt32();
                    if (rva == 0 && size == 0) return null;
                    if (rva == 0 || size == 0 || size % 28 != 0 || size > 28 * 1024)
                        throw new InvalidDataException("Invalid native PE debug directory size.");
                    long debug = Offset(r, table, sections, rva, size);
                    SymbolIdentity result = null;
                    for (uint i = 0; i < size / 28; i++) {
                        stream.Position = debug + i * 28 + 12;
                        uint type = r.ReadUInt32(), dataSize = r.ReadUInt32(); r.ReadUInt32(); uint raw = r.ReadUInt32();
                        if (type != 2) continue;
                        Range(stream, raw, dataSize);
                        if (dataSize < 4 || dataSize > 65536) throw new InvalidDataException("Invalid native PE CodeView size.");
                        stream.Position = raw;
                        if (r.ReadUInt32() != 0x53445352) continue; // RSDS; older NB10 identities are not guessed.
                        if (result != null || dataSize < 25) throw new InvalidDataException("Invalid or ambiguous native PE RSDS identity.");
                        Guid guid = new Guid(Bytes(r, 16)); uint age = r.ReadUInt32();
                        byte[] name = Bytes(r, (int)dataSize - 24); int end = Array.IndexOf(name, (byte)0);
                        if (end <= 0) throw new InvalidDataException("Invalid native PE PDB name.");
                        string basename = Path.GetFileName(new UTF8Encoding(false, true).GetString(name, 0, end));
                        if (String.IsNullOrEmpty(basename) || !basename.EndsWith(".pdb", StringComparison.OrdinalIgnoreCase) ||
                            basename.IndexOfAny(Path.GetInvalidFileNameChars()) >= 0 || basename.EndsWith(".") || basename.EndsWith(" "))
                            throw new InvalidDataException("Invalid native PE PDB basename.");
                        result = new SymbolIdentity { Name = basename, Guid = guid, Age = age };
                    }
                    return result;
                }
            } finally { stream.Position = saved; }
        }
        public static SymbolIdentity ReadPdb(Stream stream) {
            long saved = stream.Position;
            try {
                using (var r = new BinaryReader(stream, Encoding.UTF8, true)) {
                    Range(stream, 0, 56); stream.Position = 0;
                    string magic = Encoding.ASCII.GetString(Bytes(r, 32));
                    if (magic != "Microsoft C/C++ MSF 7.00\r\n\x1a" + "DS\0\0\0")
                        throw new InvalidDataException("Expected a native MSF 7 PDB.");
                    uint blockSize = r.ReadUInt32(); r.ReadUInt32(); uint blocks = r.ReadUInt32();
                    uint directorySize = r.ReadUInt32(); r.ReadUInt32(); uint map = r.ReadUInt32();
                    if (blockSize < 512 || blockSize > 65536 || (blockSize & (blockSize - 1)) != 0 ||
                        blocks == 0 || directorySize < 12 || directorySize > 32 * 1024 * 1024)
                        throw new InvalidDataException("Invalid native PDB superblock.");
                    Range(stream, 0, (long)blocks * blockSize);
                    int directoryBlocks = (int)(((long)directorySize + blockSize - 1) / blockSize);
                    Range(stream, (long)map * blockSize, directoryBlocks * 4L);
                    uint[] directoryMap = new uint[directoryBlocks]; stream.Position = (long)map * blockSize;
                    for (int i = 0; i < directoryMap.Length; i++) {
                        directoryMap[i] = r.ReadUInt32();
                        if (directoryMap[i] >= blocks) throw new InvalidDataException("Invalid native PDB directory block.");
                    }
                    using (var directory = new MemoryStream()) {
                        for (int i = 0; i < directoryMap.Length; i++) {
                            stream.Position = (long)directoryMap[i] * blockSize;
                            byte[] part = Bytes(r, (int)Math.Min(blockSize, directorySize - directory.Length));
                            directory.Write(part, 0, part.Length);
                        }
                        directory.Position = 0;
                        using (var d = new BinaryReader(directory)) {
                            uint count = d.ReadUInt32();
                            if (count < 2 || count > (directory.Length - 4) / 4)
                                throw new InvalidDataException("Invalid native PDB stream directory.");
                            uint firstSize = d.ReadUInt32(), infoSize = d.ReadUInt32();
                            if (infoSize < 28 || infoSize == UInt32.MaxValue)
                                throw new InvalidDataException("Missing native PDB identity stream.");
                            long firstBlocks = firstSize == UInt32.MaxValue ? 0 : ((long)firstSize + blockSize - 1) / blockSize;
                            long infoBlocks = ((long)infoSize + blockSize - 1) / blockSize;
                            long infoMap = 4 + count * 4L + firstBlocks * 4;
                            Range(directory, infoMap, infoBlocks * 4); directory.Position = infoMap;
                            uint infoBlock = d.ReadUInt32();
                            if (infoBlock >= blocks) throw new InvalidDataException("Invalid native PDB identity block.");
                            stream.Position = (long)infoBlock * blockSize;
                            r.ReadUInt32(); r.ReadUInt32(); uint age = r.ReadUInt32(); Guid guid = new Guid(Bytes(r, 16));
                            return new SymbolIdentity { Guid = guid, Age = age };
                        }
                    }
                }
            } finally { stream.Position = saved; }
        }
    }
}
'@
}

function Read-ClipHostCodeViewIdentity {
    [CmdletBinding(DefaultParameterSetName = 'Path')]
    param(
        [Parameter(Mandatory, ParameterSetName = 'Path')][string]$Path,
        [Parameter(Mandatory, ParameterSetName = 'Stream')][IO.Stream]$Stream
    )
    $ownsStream = $PSCmdlet.ParameterSetName -eq 'Path'
    if ($ownsStream) { $Stream = [IO.File]::OpenRead([IO.Path]::GetFullPath($Path)) }
    try { [ClipHostPackaging.NativeSymbols]::ReadImage($Stream) }
    finally { if ($ownsStream) { $Stream.Dispose() } }
}

function Assert-ClipHostNativePdb {
    [CmdletBinding()]
    param($CodeView, [Parameter(Mandatory)][IO.Stream]$Stream, [Parameter(Mandatory)][string]$Name)
    if (!$CodeView -or $Name -cne $CodeView.Name) { throw 'Native PDB name does not match the executable RSDS identity.' }
    $identity = [ClipHostPackaging.NativeSymbols]::ReadPdb($Stream)
    if ($identity.Guid -ne $CodeView.Guid -or $identity.Age -ne $CodeView.Age) {
        throw 'Native PDB GUID/age does not match the executable RSDS identity.'
    }
}
