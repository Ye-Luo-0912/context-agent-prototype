using System.Buffers.Binary;

namespace FocusAgent.Client;

/// <summary>
/// Bounded length-prefixed JSON framing shared with the Rust host:
/// 4-byte little-endian unsigned length, then exactly that many UTF-8 bytes.
/// A frame larger than <see cref="DefaultMaxFrameBytes"/> is a contract
/// violation and fails the connection — it is never truncated or skipped.
/// </summary>
public static class FrameCodec
{
    public const int HeaderBytes = 4;
    public const int DefaultMaxFrameBytes = 1 * 1024 * 1024;

    public static async Task WriteFrameAsync(
        Stream stream, ReadOnlyMemory<byte> payload, int maxFrameBytes, CancellationToken cancellationToken)
    {
        if (payload.Length > maxFrameBytes)
        {
            throw new AgentContractViolationException(
                "frame.length", $"frame is {payload.Length} bytes, above the {maxFrameBytes} byte bound");
        }
        var header = new byte[HeaderBytes];
        BinaryPrimitives.WriteUInt32LittleEndian(header, (uint)payload.Length);
        await stream.WriteAsync(header, cancellationToken).ConfigureAwait(false);
        await stream.WriteAsync(payload, cancellationToken).ConfigureAwait(false);
        await stream.FlushAsync(cancellationToken).ConfigureAwait(false);
    }

    /// <summary>Reads one whole frame. Returns null on a clean EOF at a frame boundary.</summary>
    public static async Task<byte[]?> ReadFrameAsync(
        Stream stream, int maxFrameBytes, CancellationToken cancellationToken)
    {
        var header = await ReadExactlyAsync(stream, HeaderBytes, cancellationToken).ConfigureAwait(false);
        if (header is null)
        {
            return null;
        }
        var length = BinaryPrimitives.ReadUInt32LittleEndian(header);
        if (length > (uint)maxFrameBytes)
        {
            throw new AgentContractViolationException(
                "frame.length", $"frame announces {length} bytes, above the {maxFrameBytes} byte bound");
        }
        if (length == 0)
        {
            throw new AgentContractViolationException("frame.length", "empty frames are not valid messages");
        }
        var payload = await ReadExactlyAsync(stream, (int)length, cancellationToken).ConfigureAwait(false);
        return payload ?? throw new AgentContractViolationException(
            "frame.length", "stream ended inside a frame (half frame)");
    }

    /// <summary>
    /// Reads exactly <paramref name="count"/> bytes. Half frames and short
    /// reads are never mistaken for a message; returns null only when EOF
    /// arrives cleanly at a read boundary.
    /// </summary>
    private static async Task<byte[]?> ReadExactlyAsync(
        Stream stream, int count, CancellationToken cancellationToken)
    {
        var buffer = new byte[count];
        var read = 0;
        while (read < count)
        {
            var chunk = await stream.ReadAsync(buffer.AsMemory(read, count - read), cancellationToken)
                .ConfigureAwait(false);
            if (chunk == 0)
            {
                if (read == 0)
                {
                    return null;
                }
                throw new AgentContractViolationException(
                    "frame.length", $"stream ended after {read} of {count} expected bytes");
            }
            read += chunk;
        }
        return buffer;
    }
}
