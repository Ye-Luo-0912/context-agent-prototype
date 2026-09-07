namespace FocusAgent.Client.Tests;

/// <summary>Single resolver for the shared C0 contract fixtures.</summary>
public static class ContractFixtures
{
    public static string Dir()
    {
        var probe = new[]
        {
            // Files are copied next to the test output by the csproj link.
            Path.Combine(AppContext.BaseDirectory, "Fixtures"),
            // Fallback: the canonical source of truth in the Rust crate.
            Path.GetFullPath(Path.Combine(
                AppContext.BaseDirectory, "..", "..", "..", "..", "..", "..",
                "crates", "agent-platform-protocol", "tests", "fixtures", "work")),
        };
        foreach (var candidate in probe)
        {
            if (Directory.Exists(candidate))
            {
                return candidate;
            }
        }
        throw new InvalidOperationException($"contract fixtures not found near {AppContext.BaseDirectory}");
    }

    public static string FilePath(string name) => System.IO.Path.Combine(Dir(), name);
}
