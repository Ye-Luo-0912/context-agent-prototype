using System.Net.Sockets;
using System.Security.Cryptography;
using System.Text;

namespace FocusAgent.Client;

/// <summary>One outbound connection attempt to the local host.</summary>
public interface IAgentTransport
{
    /// <summary>Human-facing description for UI display.</summary>
    string DisplayName { get; }

    /// <summary>Opens the transport and returns the bidirectional stream.</summary>
    Task<Stream> ConnectAsync(CancellationToken cancellationToken);
}

/// <summary>Windows Named Pipe transport (pipe name without the \\.\pipe\ prefix).</summary>
public sealed class NamedPipeTransport : IAgentTransport
{
    private readonly string _pipeName;
    private readonly int _connectTimeoutMs;

    public NamedPipeTransport(string pipeName, int connectTimeoutMs = 5_000)
    {
        if (string.IsNullOrWhiteSpace(pipeName))
        {
            throw new ArgumentException("pipe name must not be blank", nameof(pipeName));
        }
        _pipeName = pipeName;
        _connectTimeoutMs = connectTimeoutMs;
    }

    public string DisplayName => $"Named pipe {_pipeName}";

    public async Task<Stream> ConnectAsync(CancellationToken cancellationToken)
    {
        if (!OperatingSystem.IsWindows())
        {
            throw new PlatformNotSupportedException("Named pipes require Windows; use the Unix socket transport.");
        }
        var pipe = new System.IO.Pipes.NamedPipeClientStream(
            ".", _pipeName, System.IO.Pipes.PipeDirection.InOut, System.IO.Pipes.PipeOptions.Asynchronous);
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        timeout.CancelAfter(_connectTimeoutMs);
        await pipe.ConnectAsync(timeout.Token).ConfigureAwait(false);
        return pipe;
    }
}

/// <summary>Linux/macOS UDS transport to the host's socket path.</summary>
public sealed class UnixDomainSocketTransport : IAgentTransport
{
    private readonly string _socketPath;
    private readonly int _connectTimeoutMs;

    public UnixDomainSocketTransport(string socketPath, int connectTimeoutMs = 5_000)
    {
        if (string.IsNullOrWhiteSpace(socketPath))
        {
            throw new ArgumentException("socket path must not be blank", nameof(socketPath));
        }
        _socketPath = socketPath;
        _connectTimeoutMs = connectTimeoutMs;
    }

    public string DisplayName => $"Unix socket {_socketPath}";

    public async Task<Stream> ConnectAsync(CancellationToken cancellationToken)
    {
        // The NetworkStream takes ownership of the socket, so it must not be
        // disposed here when the connect succeeds.
        var socket = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        timeout.CancelAfter(_connectTimeoutMs);
        try
        {
            await socket.ConnectAsync(new UnixDomainSocketEndPoint(_socketPath), timeout.Token).ConfigureAwait(false);
        }
        catch
        {
            socket.Dispose();
            throw;
        }
        return new NetworkStream(socket, ownsSocket: true);
    }
}

/// <summary>Picks the platform-appropriate local transport and derives the
/// same default endpoints the Rust host binds (N4): the rule lives once in
/// the platform protocol crate (<c>workspace_endpoint_suffix</c>) and is
/// mirrored here byte-for-byte, pinned by the shared
/// <c>endpoint_derivation.json</c> fixture.</summary>
public static class AgentTransports
{
    /// <summary>Windows default named pipe (no \\.\pipe\ prefix) — the same
    /// name agent-host binds without an explicit --pipe.</summary>
    public const string DefaultPipeName = "focus-agent.platform.v1";

    /// <summary>The per-workspace endpoint discriminator: 16 hex chars of
    /// the SHA-256 digest over the workspace root's bytes as given. The
    /// PRIMITIVE stays byte-exact over whatever string it is given (the
    /// shared fixture pins exactly those bytes); resolving a workspace root
    /// to its canonical form happens in
    /// <see cref="WorkspaceIdentity.Resolve"/>, ABOVE this primitive, so
    /// both sides hash the same canonical directory rather than the same
    /// spelling.</summary>
    public static string WorkspaceEndpointSuffix(string workspaceRoot)
    {
        var digest = SHA256.HashData(Encoding.UTF8.GetBytes(workspaceRoot));
        var suffix = new StringBuilder(16);
        for (var i = 0; i < 8; i++)
        {
            suffix.Append(digest[i].ToString("x2"));
        }
        return suffix.ToString();
    }

    /// <summary>The default UDS endpoint for one workspace: the user's
    /// runtime directory when the platform provides one, else the temp dir —
    /// plus the workspace discriminator of the RESOLVED workspace root.
    /// Mirrors the host's <c>default_socket_path_for</c> for the same
    /// workspace.</summary>
    public static string DefaultSocketPathFor(string workspaceRoot)
    {
        var baseDir = Environment.GetEnvironmentVariable("XDG_RUNTIME_DIR");
        if (string.IsNullOrEmpty(baseDir))
        {
            baseDir = Path.GetTempPath();
        }
        var suffix = WorkspaceIdentity.Resolve(workspaceRoot).EndpointSuffix;
        return Path.Combine(baseDir, $"focus-agent-platform-{suffix}.sock");
    }

    /// <summary>The default local transport: the shared pipe name on
    /// Windows, the workspace-scoped UDS path (resolved from the caller's
    /// current directory — the host's own default workspace choice) on
    /// Unix.</summary>
    public static IAgentTransport DefaultLocal() =>
        OperatingSystem.IsWindows()
            ? new NamedPipeTransport(DefaultPipeName)
            : new UnixDomainSocketTransport(DefaultSocketPathFor(Environment.CurrentDirectory));

    /// <summary>The endpoint string the default transport would use on this
    /// platform (pipe name, or derived socket path).</summary>
    public static string DefaultEndpoint() =>
        OperatingSystem.IsWindows()
            ? DefaultPipeName
            : DefaultSocketPathFor(Environment.CurrentDirectory);
}

/// <summary>PLATFORM-3: one resolved workspace binding — the canonical
/// absolute root, the endpoint discriminator derived from it, and the human
/// display string. The shared rule both sides follow: the discriminator
/// hashes the RESOLVED root; each side canonicalizes with its own platform
/// semantics and the host's bind is authoritative. A client that must be
/// certain which workspace answered compares this root against
/// <see cref="WorkSnapshotResponse.WorkspaceRoot"/> — the snapshot carries
/// the host's own canonical form, which no client-side guess can override.
/// </summary>
public sealed record WorkspaceIdentity(string Root, string EndpointSuffix)
{
    public string Display => $"workspace {Root}";

    public static WorkspaceIdentity Resolve(string workspaceRoot)
    {
        if (string.IsNullOrWhiteSpace(workspaceRoot))
        {
            throw new ArgumentException("workspace root must not be blank", nameof(workspaceRoot));
        }
        var root = ResolveCanonical(workspaceRoot);
        return new WorkspaceIdentity(root, AgentTransports.WorkspaceEndpointSuffix(root));
    }

    /// <summary>Best-effort mirror of the host's <c>std::fs::canonicalize</c>:
    /// make the path absolute, drop redundant components, and follow the
    /// final symlink/junction target when the path exists. Every step that
    /// cannot complete keeps the previous form — never a guess beyond what
    /// the platform can prove; the snapshot's host-side root remains the
    /// authority for equality.</summary>
    private static string ResolveCanonical(string path)
    {
        var full = Path.GetFullPath(path);
        var root = Path.GetPathRoot(full) ?? string.Empty;
        // Trim a trailing separator only when more than the root remains:
        // "C:\" must never collapse to the drive-relative "C:".
        if (full.Length > root.Length)
        {
            full = full.TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
        }
        try
        {
            if (Directory.Exists(full))
            {
                var target = new DirectoryInfo(full)
                    .ResolveLinkTarget(returnFinalTarget: true)
                    ?.FullName;
                if (target is not null)
                {
                    full = target;
                }
            }
        }
        catch (IOException)
        {
            // A link that cannot be resolved keeps the current form.
        }
        return full;
    }
}
