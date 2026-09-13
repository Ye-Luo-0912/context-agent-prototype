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
                if (viewModel.LogText.Contains("工具事件")
                    && viewModel.Tasks.Any(task => task.Goal == "demo: list files"))
                {
                    break;
                }
                await Task.Delay(250);
            }

            Assert.True(
                viewModel.LogText.Contains("工具事件"),
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

    /// <summary>
    /// C3: the review surface against the REAL host — the four B3 read-only
    /// routes (task detail / change journal / artifact bytes / read-only
    /// context) driven through the REAL <see cref="ResumableSession"/> and the
    /// desktop workbench, exactly like the production connect path. Everything
    /// is observation: submits and approvals are never touched by this drill.
    /// The task detail arrives over the wire (goal visible), the change
    /// journal reflects whatever the demo run actually journaled (honest count,
    /// possibly zero), a never-sealed artifact reference is refused by the
    /// host (the panel says unavailable — fail-closed, never a guessed body),
    /// and the engine's context summary is non-empty after the demo submit
    /// (the goal reached the engine). Passes as NOT_RUN without the debug host.
    /// </summary>
    [Fact]
    public async Task Real_host_review_routes_drive_the_review_surface()
    {
        var hostBinary = FindHostBinary();
        if (hostBinary is null)
        {
            return; // NOT_RUN: this environment has no debug agent-host build
        }

        var workdir = Path.Combine(Path.GetTempPath(), $"n8-review-{Guid.NewGuid():N}");
        Directory.CreateDirectory(workdir);
        var pipeName = $"n8-review-{Guid.NewGuid():N}";
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

            await using var viewModel = new MainWindowViewModel(new InlineUiDispatcher());
            await viewModel.ConnectSessionForTestsAsync(() =>
                OperatingSystem.IsWindows()
                    ? new NamedPipeTransport(pipeName).ConnectAsync(CancellationToken.None)
                    : new UnixDomainSocketTransport(socketPath).ConnectAsync(CancellationToken.None))
                .WaitAsync(TimeSpan.FromSeconds(120));
            Assert.True(
                viewModel.IsConnected,
                $"the workbench never connected; output:\n{viewModel.OutputText}");

            // A real submit lands the task in the engine; the review reads
            // below then observe the host's own facts (this drill performs no
            // further mutation).
            viewModel.GoalInput = "demo: list files";
            await viewModel.SubmitForTestsAsync();
            await WaitUntilAsync(
                () => viewModel.Tasks.Any(task => task.Goal == "demo: list files"),
                "the submitted task in the snapshot-driven list");

            // 1. task_detail over the wire: the goal arrives verbatim from the
            // host (B3 host e2e proves the same fact for the Rust side).
            await viewModel.LoadTaskDetailForTestsAsync(viewModel.Tasks.First(t => t.Goal == "demo: list files").TaskId);
            Assert.Contains("demo: list files", viewModel.TaskDetailText);

            // 2. changes over the wire: the panel reflects whatever the demo
            // run journaled. Host journal for a read-only demo tool is often
            // empty; the panel must say so honestly (count, not prose).
            await viewModel.RefreshChangesForTestsAsync();
            Assert.Contains("条", viewModel.ChangesStatusText);
            Assert.DoesNotContain("读取失败", viewModel.ChangesStatusText);

            // 3. artifact over the wire: a reference that was never sealed is
            // refused by the host (fail-closed) — the panel stays honest,
            // never a guessed body.
            await viewModel.ReadArtifactForTestsAsync("artifact://run/never-sealed");
            Assert.Contains("unavailable", viewModel.ArtifactText);
            Assert.DoesNotContain("（完整）", viewModel.ArtifactText);

            // 4. context over the wire: the submitted goal reached the
            // engine, so the read-only summary is non-empty (B3 host e2e
            // proves the same for the Rust side).
            await viewModel.RefreshContextForTestsAsync();
            Assert.NotEmpty(viewModel.ContextItems);
            Assert.DoesNotContain("读取失败", viewModel.ContextStatusText);
        }
        finally
        {
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

    /// <summary>
    /// PLATFORM-1 (F06): the exact-request receipt query against the REAL
    /// host through the production <see cref="ResumableSession"/> SDK. The
    /// admitted id reads back as Accepted bound to its task; the same id with
    /// a different payload is an explicit KnownRejected conflict; an unseen
    /// id — even one whose GOAL text matches the recorded submission — is
    /// Unknown, never a negative proof. This is the query GUI-3 will consume
    /// instead of matching goals in snapshots. Passes as NOT_RUN without the
    /// debug host.
    /// </summary>
    [Fact]
    public async Task Real_host_answers_the_exact_submission_receipt_query()
    {
        var hostBinary = FindHostBinary();
        if (hostBinary is null)
        {
            return; // NOT_RUN: this environment has no debug agent-host build
        }

        var workdir = Path.Combine(Path.GetTempPath(), $"p1-receipt-{Guid.NewGuid():N}");
        Directory.CreateDirectory(workdir);
        var pipeName = $"p1-receipt-{Guid.NewGuid():N}";
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
                        : " the host is still running"));

            await using var session = new ResumableSession(() =>
                OperatingSystem.IsWindows()
                    ? new NamedPipeTransport(pipeName).ConnectAsync(CancellationToken.None)
                    : new UnixDomainSocketTransport(socketPath).ConnectAsync(CancellationToken.None));

            // A real admission first: the receipt the submit returns is the
            // same fact the query must reproduce later (ACK-lost reconnect).
            const string goal = "demo: receipt query";
            var submit = await session.SubmitWorkAsync(goal, "receipt-probe-1")
                .WaitAsync(TimeSpan.FromSeconds(30));
            Assert.Equal(WorkSubmitDisposition.Accepted, submit.Disposition);

            var admitted = await session.SubmitResultAsync(
                "receipt-probe-1", SubmitPayloadDigest.Compute(goal))
                .WaitAsync(TimeSpan.FromSeconds(30));
            Assert.Equal(WorkSubmitResultDisposition.Accepted, admitted.Disposition);
            Assert.True(admitted.Disposition.IsAdmitted());
            Assert.Equal(submit.TaskId, admitted.TaskId);
            Assert.Equal("receipt-probe-1", admitted.ClientRequestId);
            Assert.False(string.IsNullOrEmpty(admitted.RunId));

            // Same id, different payload: an explicit terminal conflict that
            // reports what the id WAS admitted for — never a fresh execution.
            var conflict = await session.SubmitResultAsync(
                "receipt-probe-1", SubmitPayloadDigest.Compute("a different goal"))
                .WaitAsync(TimeSpan.FromSeconds(30));
            Assert.Equal(WorkSubmitResultDisposition.KnownRejected, conflict.Disposition);
            Assert.Equal(
                SubmitPayloadDigest.Compute(goal), conflict.AcceptedPayloadDigest);
            Assert.Equal(submit.TaskId, conflict.TaskId);

            // A different id whose goal TEXT matches the recorded submission
            // is still Unknown: the goal is not the idempotency key (F06).
            var sameGoalNewId = await session.SubmitResultAsync(
                "receipt-probe-never-admitted", SubmitPayloadDigest.Compute(goal))
                .WaitAsync(TimeSpan.FromSeconds(30));
            Assert.Equal(WorkSubmitResultDisposition.Unknown, sameGoalNewId.Disposition);
            Assert.True(sameGoalNewId.Disposition.IsIndeterminate());
            Assert.Null(sameGoalNewId.TaskId);
            Assert.Null(sameGoalNewId.AcceptedPayloadDigest);
        }
        finally
        {
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

    private static async Task WaitUntilAsync(Func<bool> condition, string what)
    {
        var deadline = DateTimeOffset.UtcNow + TimeSpan.FromSeconds(90);
        while (!condition())
        {
            Assert.True(DateTimeOffset.UtcNow < deadline, $"timed out waiting for {what}");
            await Task.Delay(250);
        }
    }

    /// <summary>
    /// M18 COST-5 前置：真实 provider（DeepSeek Flash）端到端——正式 agent-host
/// 二进制（无 AGENT_DEMO，OPENAI_* 来自 eval.env 注入的进程环境）＋生产
/// SDK 面（ResumableSession）：提交真实小任务 → 工具落盘 → TurnCompleted →
/// 工件断言 → PLATFORM-1 精确收据查询（Accepted/Unknown 双臂）。密钥不进
/// 仓库：eval.env 缺失或无 OPENAI_API_KEY 时 NOT_RUN（不失败）。
/// </summary>
[Fact]
public async Task Real_provider_host_end_to_end_with_deepseek_flash()
{
    // 1. eval.env：只从仓库根读（gitignored），密钥不进代码/报告。
    var repoRoot = FindRepoRoot();
    if (repoRoot is null)
    {
        return; // NOT_RUN: repo root not found
    }
    var envFile = Path.Combine(repoRoot, "eval.env");
    if (!File.Exists(envFile))
    {
        return; // NOT_RUN: no eval.env in this environment
    }
    var env = new Dictionary<string, string>();
    foreach (var raw in File.ReadAllLines(envFile))
    {
        var line = raw.Trim();
        if (line.Length == 0 || line.StartsWith('#'))
        {
            continue;
        }
        var eq = line.IndexOf('=');
        if (eq > 0)
        {
            env[line[..eq].Trim()] = line[(eq + 1)..].Trim();
        }
    }
    if (!env.ContainsKey("OPENAI_API_KEY"))
    {
        return; // NOT_RUN: no key configured
    }

    // 2. Spawn the REAL host binary with the provider env (no AGENT_DEMO).
    var workdir = Path.Combine(Path.GetTempPath(), $"p5-live-{Guid.NewGuid():N}");
    Directory.CreateDirectory(workdir);
    var pipeName = $"p5-live-{Guid.NewGuid():N}";
    var socketPath = Path.Combine(workdir, "host.sock");
    var args = OperatingSystem.IsWindows()
        ? $"--workdir \"{workdir}\" --pipe {pipeName}"
        : $"--workdir \"{workdir}\" --socket \"{socketPath}\"";
    var start = new ProcessStartInfo
    {
        FileName = FindHostBinary()!,
        Arguments = args,
        UseShellExecute = false,
        RedirectStandardOutput = true,
        RedirectStandardError = true,
    };
    start.Environment["OPENAI_BASE_URL"] = env.GetValueOrDefault("OPENAI_BASE_URL", "");
    start.Environment["OPENAI_MODEL"] = env.GetValueOrDefault("OPENAI_MODEL", "");
    start.Environment["OPENAI_API_PROTOCOL"] = env.GetValueOrDefault("OPENAI_API_PROTOCOL", "auto");
    start.Environment["OPENAI_API_KEY"] = env["OPENAI_API_KEY"];
    if (env.TryGetValue("OPENAI_MAX_OUTPUT_TOKENS", out var maxOut))
    {
        start.Environment["OPENAI_MAX_OUTPUT_TOKENS"] = maxOut;
    }

    using var process = Process.Start(start)!;
    var hostOutput = new System.Text.StringBuilder();
    process.OutputDataReceived += (_, e) => hostOutput.AppendLine(e.Data);
    process.ErrorDataReceived += (_, e) => hostOutput.AppendLine(e.Data);
    process.BeginOutputReadLine();
    process.BeginErrorReadLine();
    await using var spawned = new SpawnedHost { Process = process, Workdir = workdir };
    try
    {
        var ready = await WaitForEndpointAsync(socketPath, pipeName, TimeSpan.FromSeconds(60));
        Assert.True(ready, "the host endpoint never became connectable");

        await using var session = new ResumableSession(() =>
            OperatingSystem.IsWindows()
                ? new NamedPipeTransport(pipeName).ConnectAsync(CancellationToken.None)
                : new UnixDomainSocketTransport(socketPath).ConnectAsync(CancellationToken.None));

        // 3. A real small task: create one file with exact content.
        const string goal =
            "Create a file named hello.txt in the workspace root whose entire content is exactly HELLO-FROM-DEEPSEEK (no trailing newline). Use the workspace tools.";
        var submit = await session.SubmitWorkAsync(goal, "p5-live-1")
            .WaitAsync(TimeSpan.FromSeconds(30));
        Assert.Equal(WorkSubmitDisposition.Accepted, submit.Disposition);

        // 4. Wait for the artifact itself (real model + tools, bounded).
        // NOTE: OperatorClosureOnly means the model can never flip the task
        // to Completed — the operator does that explicitly. The artifact is
        // the honest completion signal for this walkthrough.
        var produced = Path.Combine(workdir, "hello.txt");
        var deadline = DateTimeOffset.UtcNow + TimeSpan.FromSeconds(240);
        while (DateTimeOffset.UtcNow < deadline)
        {
            // The host runs an interactive approval gate: the operator (this
            // test) must Allow each tool call before it executes — that is
            // the product's informed-approval semantics, not a test hack.
            var snapshot = await session.SnapshotAsync().WaitAsync(TimeSpan.FromSeconds(30));
            foreach (var approval in snapshot.PendingApprovals)
            {
                await session.RespondApprovalAsync(
                    approval.RequestId, ApprovalDecision.Allow)
                    .WaitAsync(TimeSpan.FromSeconds(30));
            }
            if (File.Exists(produced)
                && File.ReadAllText(produced).TrimEnd('\r', '\n') == "HELLO-FROM-DEEPSEEK")
            {
                break;
            }
            await Task.Delay(1000);
        }
        Assert.True(
            File.Exists(produced)
                && File.ReadAllText(produced).TrimEnd('\r', '\n') == "HELLO-FROM-DEEPSEEK",
            "the live task did not produce hello.txt with the exact content; "
                + $"hostExited={process.HasExited} "
                + $"| tasks=[{string.Join("; ", (await session.SnapshotAsync().WaitAsync(TimeSpan.FromSeconds(30))).Tasks.Select(t => $"{t.Goal[..60]}→{t.Status}"))}]"
                + $"| hostOutput:\n{hostOutput}" // DIAG
        );

        // 6. PLATFORM-1 over the same live run: the admitted id reads back
        // Accepted; a never-seen id reads Unknown — never a negative proof.
        var receipt = await session.SubmitResultAsync("p5-live-1")
            .WaitAsync(TimeSpan.FromSeconds(30));
        Assert.Equal(WorkSubmitResultDisposition.Accepted, receipt.Disposition);
        Assert.Equal(submit.TaskId, receipt.TaskId);
        var unseen = await session.SubmitResultAsync("p5-live-never")
            .WaitAsync(TimeSpan.FromSeconds(30));
        Assert.Equal(WorkSubmitResultDisposition.Unknown, unseen.Disposition);
        Assert.True(unseen.Disposition.IsIndeterminate());
    }
    finally
    {
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

private static string? FindRepoRoot()
{
    var dir = AppContext.BaseDirectory;
    while (dir is not null && !File.Exists(Path.Combine(dir, "Cargo.toml")))
    {
        dir = Path.GetDirectoryName(dir);
    }
    return dir;
}
}
