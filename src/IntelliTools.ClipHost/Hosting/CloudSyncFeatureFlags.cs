namespace IntelliTools.ClipHost.Hosting;

/// <summary>
/// Temporary product gate for favor cloud sync. Set <see cref="IsPubliclyAvailable"/> to true to re-enable UI and runtime sync.
/// </summary>
public static class CloudSyncFeatureFlags
{
    public const bool IsPubliclyAvailable = false;
}
