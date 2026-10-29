# Validate immutable PE metadata separately from the external package manifest.
# Dot-sourcing defines this check only; it never builds or starts a host.
function Assert-ClipHostStandaloneBuildInfo {
    param(
        [Parameter(Mandatory)][System.Collections.IDictionary]$BuildInfo,
        [Parameter(Mandatory)][ValidateSet('Gpui', 'Tauri')][string]$Renderer,
        [string]$OptLevel
    )
    if ($BuildInfo['format'] -cne 'cliphost-build-v1' -or
        $BuildInfo['native_acceptance'] -cne 'false' -or
        @('1', '2', '3', 's', 'z') -cnotcontains [string]$BuildInfo['opt_level']) {
        throw 'The executable must be an optimized standalone build, not an acceptance fixture.'
    }
    if ($OptLevel -and $BuildInfo['opt_level'] -cne $OptLevel) {
        throw 'The actual executable does not match the optimized standalone build log.'
    }
    if ($Renderer -eq 'Gpui') {
        if ($BuildInfo['renderer'] -cne 'gpui' -or
            $BuildInfo['embedded_frontend'] -cne 'false' -or
            $BuildInfo['custom_protocol'] -cne 'false') {
            throw 'The executable is not a native GPUI standalone build.'
        }
    } elseif ($BuildInfo['custom_protocol'] -cne 'true' -or
        ($BuildInfo.Contains('renderer') -and $BuildInfo['renderer'] -cne 'tauri') -or
        ($BuildInfo.Contains('embedded_frontend') -and $BuildInfo['embedded_frontend'] -cne 'true')) {
        # Tauri build-v1 resources predate explicit renderer/asset fields.
        throw 'The executable is not a Tauri standalone build with embedded frontend assets.'
    }
}
