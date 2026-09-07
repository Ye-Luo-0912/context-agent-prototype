using System.Net.Sockets;

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

/// <summary>Picks the platform-appropriate local transport.</summary>
public static class AgentTransports
{
    /// <summary>Default local endpoints: pipe name (Windows) or socket path (Unix).</summary>
    public const string DefaultPipeName = "focus-agent.platform.v1";
    public const string DefaultSocketPath = "/tmp/focus-agent-platform-v1.sock";

    public static IAgentTransport DefaultLocal() =>
        OperatingSystem.IsWindows()
            ? new NamedPipeTransport(DefaultPipeName)
            : new UnixDomainSocketTransport(DefaultSocketPath);
}
