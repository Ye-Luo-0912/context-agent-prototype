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

var clientRequestId = ClientRequestIds.Next();
var receipt = await connection.SubmitWorkAsync(
    "interop smoke: verify the client host handshake", clientRequestId);
Console.WriteLine($"submit: {receipt.Disposition} task={receipt.TaskId}");

var after = await connection.SnapshotAsync();
Console.WriteLine($"snapshot after submit: focus={(after.Focus is null ? "none" : after.Focus.TaskId)} tasks={after.Tasks.Count}");

// Idempotent retry: the SAME client_request_id with the same goal must
// return the original admission, never a second task.
var retry = await connection.SubmitWorkAsync(
    "interop smoke: verify the client host handshake", clientRequestId);
Console.WriteLine($"idempotent retry: {retry.Disposition} task={retry.TaskId}");
if (retry.Disposition != WorkSubmitDisposition.AlreadyAccepted || retry.TaskId != receipt.TaskId)
{
    Console.Error.WriteLine(
        $"idempotent retry broke admission: got {retry.Disposition} task={retry.TaskId}, original task={receipt.TaskId}");
    return 1;
}

var cancel = await connection.CancelCurrentTurnAsync();
Console.WriteLine($"cancel: {cancel.Ack.Status}");

Console.WriteLine("interop smoke OK");
return 0;
