using FocusAgent.Client;

// G2 interop smoke: the real .NET client against the real Rust host's
// default named pipe. Exits 0 only when the full work plane answers.
var pipe = args.Length > 0 ? args[0] : AgentTransports.DefaultPipeName;
var transport = new NamedPipeTransport(pipe);
var stream = await transport.ConnectAsync(CancellationToken.None);
await using var connection = new AgentConnection(stream);
Console.WriteLine($"connected: {pipe}");

var snapshot = await connection.SnapshotAsync();
Console.WriteLine($"snapshot: run_started={snapshot.RunStarted} watermark={snapshot.Watermark}");

var receipt = await connection.SubmitWorkAsync(
    "interop smoke: verify the client host handshake", ClientRequestIds.Next());
Console.WriteLine($"submit: {receipt.Disposition} task={receipt.TaskId}");

var after = await connection.SnapshotAsync();
Console.WriteLine($"snapshot after submit: focus={(after.Focus is null ? "none" : after.Focus.TaskId)} tasks={after.Tasks.Count}");

var retry = await connection.SubmitWorkAsync(
    "interop smoke: verify the client host handshake", ClientRequestIds.Next());
Console.WriteLine($"unrelated new submission: {retry.Disposition} task={retry.TaskId}");

var cancel = await connection.CancelCurrentTurnAsync();
Console.WriteLine($"cancel: {cancel.Ack.Status}");

Console.WriteLine("interop smoke OK");
