using System.Diagnostics;
using System.IO.Pipes;
using System.Net.Sockets;

using FocusAgent.Desktop.Infrastructure;
using FocusAgent.Desktop.ViewModels;

using Xunit;

namespace FocusAgent.Client.Tests;

/// <summary>
/// N8: the real cross-binary chain — a real .NET <see cref="ResumableSession"/>
/// (and the desktop workbench wired exactly like production) against the
/// REAL <c>agent-host</c> binary running a real Runtime with the scripted
/// demo model (AGENT_DEMO=1), driving a REAL tool call (<c>fs.list</c>)
/// whose typed event reaches the GUI's event pump. This is the "scripted
/// model" full chain: .NET → Rust host → Runtime → Tool → Event → GUI.
///
/// The test needs the debug <c>agent-host</c> binary; when it is not present
/// it passes as NOT_RUN (nothing asserted), so the suite stays green in
/// environments that did not build Rust.
/// </summary>
public class HostChainTests
{
    private static string? FindHostBinary()
    {
        var fromEnv = Environment.GetEnvironmentVariable("AGENT_HOST_BIN");
        if (!string.IsNullOrEmpty(fromEnv) && File.Exists(fromEnv))
        {
            return fromEnv;
        }
        var dir = AppContext.BaseDirectory;
        while (dir is not null && !File.Exists(Path.Combine(dir, "Cargo.toml")))
        {
            dir = Path.GetDirectoryName(dir);
        }
        if (dir is null)
        {
            return null;
        }
        var name = OperatingSystem.IsWindows() ? "agent-host.exe" : "agent-host";
        var candidate = Path.Combine(dir, "target", "debug", name);
        return File.Exists(candidate) ? candidate : null;
    }

    private static async Task<bool> WaitForEndpointAsync(string endpoint, string pipeName, TimeSpan budget)
    {
        var deadline = DateTimeOffset.UtcNow + budget;
        while (DateTimeOffset.UtcNow < deadline)
        {
            try
            {
                if (OperatingSystem.IsWindows())
                {
                    using var client = new NamedPipeClientStream(".", pipeName, PipeDirection.InOut);
                    client.Connect(100);
                    return true;
                }
                using (var socket = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified))
                {
                    socket.Connect(new UnixDomainSocketEndPoint(endpoint));
                    return true;
                }
            }
            catch
            {
                await Task.Delay(200);
            }
        }
        return false;
    }

    /// <summary>One host process + one temp workspace, cleaned up even when
    /// the assertion path fails.</summary>
    private sealed class SpawnedHost : IAsyncDisposable
    {
        public required Process Process { get; init; }
        public required string Workdir { get; init; }

        public async ValueTask DisposeAsync()
        {
            try
            {
                if (!Process.HasExited)
                {
                    Process.Kill(entireProcessTree: true);
                }
                await Process.WaitForExitAsync().WaitAsync(TimeSpan.FromSeconds(10));
            }
            catch
            {
                // Test teardown: the host is already gone or cannot be killed.
            }
            Process.Dispose();
            try
            {
                Directory.Delete(Workdir, recursive: true);
            }
            catch
            {
            }
        }
    }

    [Fact]
    public async Task Real_host_runtime_tool_event_reaches_the_desktop_workbench()
    {
        var hostBinary = FindHostBinary();
        if (hostBinary is null)
        {
            return; // NOT_RUN: this environment has no debug agent-host build
        }

        var workdir = Path.Combine(Path.GetTempPath(), $"n8-chain-{Guid.NewGuid():N}");
        Directory.CreateDirectory(workdir);
        // Unique endpoint per run: parallel tests must never collide on the
        // fixed default pipe.
        var pipeName = $"n8-chain-{Guid.NewGuid():N}";
        var socketPath = Path.Combine(workdir, "host.sock");
        var args = OperatingSystem.IsWindows()
            ? $"--workdir \"{workdir}\" --pipe {pipeName}"
            : $"--workdir \"{workdir}\" --socket \"{socketPath}\"";

        var start = new ProcessStartInfo
        {
            FileName = hostBinary,
            Arguments = args,
            UseShellExecute = false,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
        };
        start.Environment["AGENT_DEMO"] = "1";
        using var process = Process.Start(start)!;
        await using var spawned = new SpawnedHost { Process = process, Workdir = workdir };
        try
        {
            var ready = await WaitForEndpointAsync(
                socketPath, pipeName, TimeSpan.FromSeconds(60));
            Assert.True(
                ready,
                $"the host endpoint never became connectable;"
                    + (process.HasExited
                        ? $" stderr:\n{await process.StandardError.ReadToEndAsync()}"
                        : " the host is still running (its stderr is only read after exit)"));

            // The desktop workbench over the REAL host connection, wired
            // exactly like production (resync banner, event pump, snapshot).
            await using var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
            Exception? connectFailure = null;
            try
            {
                await viewModel.ConnectSessionForTestsAsync(() =>
                    OperatingSystem.IsWindows()
                        ? new NamedPipeTransport(pipeName).ConnectAsync(CancellationToken.None)
                        : new UnixDomainSocketTransport(socketPath).ConnectAsync(CancellationToken.None))
                    .WaitAsync(TimeSpan.FromSeconds(120));
            }
            catch (Exception failure)
            {
                connectFailure = failure;
            }
            Assert.True(
                viewModel.IsConnected,
                $"the workbench never connected ({(connectFailure is null ? "no throw" : $"threw {connectFailure}")}); "
                    + $"output:\n{viewModel.OutputText}"
                    + (process.HasExited
                        ? $"\n\nhost stderr:\n{await process.StandardError.ReadToEndAsync()}"
                        : "\n\n(host still running; its stderr is only read after exit)"));

            // The scripted demo model turns this submit into a REAL fs.list
            // tool call inside the host's Runtime; the tool event must reach
            // the GUI's typed event pump and the task must appear in the
            // snapshot-driven task list.
            viewModel.GoalInput = "demo: list files";
            await viewModel.SubmitForTestsAsync();

            var deadline = DateTimeOffset.UtcNow + TimeSpan.FromSeconds(90);
            while (DateTimeOffset.UtcNow < deadline)
            {
                if (viewModel.OutputText.Contains("工具事件")
                    && viewModel.Tasks.Any(task => task.Goal == "demo: list files"))
                {
                    break;
                }
                await Task.Delay(250);
            }

            Assert.True(
                viewModel.OutputText.Contains("工具事件"),
                $"the tool event never reached the GUI; output:\n{viewModel.OutputText}");
            Assert.Contains(viewModel.Tasks, task => task.Goal == "demo: list files");
        }
        finally
        {
            // Ensure the host is gone so the process object can exit cleanly
            // on teardown.
            try
            {
                if (!process.HasExited)
                {
                    process.Kill(entireProcessTree: true);
                }
            }
            catch
            {
            }
        }
    }
}
