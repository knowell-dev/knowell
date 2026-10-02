namespace Example.Depot;

public static class Settings
{
    public static string? Region() => System.Environment.GetEnvironmentVariable("DEPOT_CS_REGION");
}
