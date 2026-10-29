# Reads PE resources as data only; this does not start the program or its imports.
function Read-ClipHostExecutableBuildInfo {
    param([Parameter(Mandatory)][string]$Path)

    if (!('ClipHostExecutableResources' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

public static class ClipHostExecutableResources
{
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr LoadLibraryExW(string path, IntPtr file, uint flags);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr FindResourceW(IntPtr module, string name, IntPtr type);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint SizeofResource(IntPtr module, IntPtr resource);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr LoadResource(IntPtr module, IntPtr resource);
    [DllImport("kernel32.dll")]
    private static extern IntPtr LockResource(IntPtr resource);
    [DllImport("kernel32.dll")]
    private static extern bool FreeLibrary(IntPtr module);

    public static string Read(string path)
    {
        // LOAD_LIBRARY_AS_DATAFILE_EXCLUSIVE | LOAD_LIBRARY_AS_IMAGE_RESOURCE.
        var module = LoadLibraryExW(path, IntPtr.Zero, 0x60);
        if (module == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
        try
        {
            var resource = FindResourceW(module, "CLIPHOST_BUILD_INFO", new IntPtr(10));
            if (resource == IntPtr.Zero)
                throw new InvalidOperationException("Executable build metadata is missing; rebuild ClipHost before packaging.");
            uint size = SizeofResource(module, resource);
            if (size == 0 || size > 4096)
                throw new InvalidOperationException("Invalid executable build metadata size.");
            var loaded = LoadResource(module, resource);
            var pointer = LockResource(loaded);
            if (loaded == IntPtr.Zero || pointer == IntPtr.Zero)
                throw new Win32Exception(Marshal.GetLastWin32Error());
            var bytes = new byte[size];
            Marshal.Copy(pointer, bytes, 0, bytes.Length);
            return new UTF8Encoding(false, true).GetString(bytes);
        }
        finally { FreeLibrary(module); }
    }
}
'@
    }
    $info = ConvertFrom-StringData ([ClipHostExecutableResources]::Read([IO.Path]::GetFullPath($Path)))
    if ($info.format -cne 'cliphost-build-v1') { throw 'Unrecognized executable build metadata.' }
    return $info
}
