//! Windows Named Pipe backend for the local host.
//!
//! Two independent fail-closed layers gate a connection:
//! 1. the pipe is created with `PIPE_REJECT_REMOTE_CLIENTS` and a DACL
//!    granting exactly the current user general access, so remote sessions
//!    and other local accounts cannot reach it at all;
//! 2. each accepted connection's client process token user SID is compared
//!    with the host's own; a mismatch or any failure inside the check drops
//!    the peer before its first frame is read.
//!
//! Framing, decoding, routing and session discipline are shared with the
//! UDS backend ([`super`]).

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT, PeekNamedPipe,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::{
    CancelHook, ConnectionEvents, HostPlane, LiveStreams, MAX_CONNECTIONS, SHUTDOWN_GRACE,
    open_connection_plane, process_connection, session_unavailable_frame, write_frame,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// How long the request loop waits between pipe peeks while no request has
/// arrived. Bounds both the added request latency and the wind-down time of
/// an idle connection; the loop holds no pending I/O while sleeping, which
/// is what keeps the event forwarder's writes unblocked.
const REQUEST_POLL: Duration = Duration::from_millis(5);

/// A TOKEN_USER query result: the raw buffer (which owns the SID) plus the
/// SID pointer into it. The buffer must outlive every use of [`Self::sid`].
struct TokenUserBuffer {
    buffer: Vec<u8>,
}

impl TokenUserBuffer {
    fn query(token: HANDLE) -> std::io::Result<Self> {
        unsafe {
            let mut returned = 0u32;
            // Size discovery; the information output is intentionally ignored.
            _ = GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut returned);
            if returned == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut buffer = vec![0u8; returned as usize];
            if GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                &mut returned,
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self { buffer })
        }
    }

    /// The SID pointer into the owned buffer.
    fn sid(&self) -> Option<PSID> {
        let user = self.buffer.as_ptr() as *const TOKEN_USER;
        unsafe {
            let sid = (*user).User.Sid;
            if sid.is_null() {
                None
            } else {
                Some(sid.cast())
            }
        }
    }
}

fn open_own_token() -> std::io::Result<OwnedHandle> {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(OwnedHandle::from_raw_handle(token))
    }
}

/// The host's own TOKEN_USER buffer, queried once.
fn host_token_user() -> std::io::Result<&'static TokenUserBuffer> {
    static HOST: OnceLock<std::io::Result<TokenUserBuffer>> = OnceLock::new();
    // OnceLock of Result: a failed query is cached as Err and re-raised.
    let cell = HOST.get_or_init(|| {
        let token = open_own_token()?;
        TokenUserBuffer::query(token.as_raw_handle() as _)
    });
    match cell {
        Ok(buffer) => {
            // SAFETY of the 'static: the buffer lives in the process-lifetime OnceLock.
            Ok(unsafe { &*(buffer as *const TokenUserBuffer) })
        }
        Err(error) => Err(std::io::Error::new(error.kind(), error.to_string())),
    }
}

/// The current user's SID in SDDL string form (for the pipe DACL).
fn current_user_sid() -> std::io::Result<String> {
    let buffer = host_token_user()?;
    let Some(sid) = buffer.sid() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "token user sid missing",
        ));
    };
    unsafe {
        let mut text: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut text) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut length = 0usize;
        while *text.add(length) != 0 {
            length += 1;
        }
        let value = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        LocalFree(text.cast());
        Ok(value)
    }
}

/// SECURITY_DESCRIPTOR: `D:(A;;GA;;;<current user SID>)` — only this
/// account may open the pipe at all.
struct UserOnlySecurity {
    descriptor: *mut core::ffi::c_void,
}

const SDDL_REVISION: u32 = 1;

impl UserOnlySecurity {
    fn new() -> std::io::Result<Self> {
        let sddl = wide(&format!("D:(A;;GA;;;{})", current_user_sid()?));
        let mut descriptor: *mut core::ffi::c_void = std::ptr::null_mut();
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { descriptor })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        }
    }
}

impl Drop for UserOnlySecurity {
    fn drop(&mut self) {
        unsafe { LocalFree(self.descriptor.cast()) };
    }
}

fn create_pipe_instance(
    name: &[u16],
    security: &UserOnlySecurity,
    first_instance: bool,
) -> std::io::Result<OwnedHandle> {
    let attributes = security.attributes();
    // FILE_FLAG_FIRST_PIPE_INSTANCE belongs only on the very first instance:
    // it fails creation while any other instance of the pipe name is still
    // open, which is exactly the state of every later accept-loop iteration
    // (the previous instance lives in its connection worker). The name
    // reservation is established by the first create and then persists.
    let open_flags = if first_instance {
        PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        PIPE_ACCESS_DUPLEX
    };
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_flags,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            super::MAX_FRAME_BYTES,
            super::MAX_FRAME_BYTES,
            0,
            &attributes,
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Client-token check: the connected peer's user SID must equal the host's.
fn verify_pipe_peer(handle: HANDLE) -> bool {
    let Ok(host) = host_token_user() else {
        return false;
    };
    let Some(host_sid) = host.sid() else {
        return false;
    };
    unsafe {
        let mut pid: u32 = 0;
        if GetNamedPipeClientProcessId(handle, &mut pid) == 0 || pid == 0 {
            return false;
        }
        let client = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if client.is_null() || client == INVALID_HANDLE_VALUE {
            return false;
        }
        let client_owned = OwnedHandle::from_raw_handle(client);
        let Ok(client_token) = open_process_token(client_owned.as_raw_handle() as _) else {
            return false;
        };
        let Ok(client_user) = TokenUserBuffer::query(client_token.as_raw_handle() as _) else {
            return false;
        };
        let Some(client_sid) = client_user.sid() else {
            return false;
        };
        EqualSid(client_sid, host_sid) != 0
    }
}

unsafe fn open_process_token(process: HANDLE) -> std::io::Result<OwnedHandle> {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(OwnedHandle::from_raw_handle(token))
    }
}

pub(super) fn serve(
    name: String,
    read_only: bool,
    plane: HostPlane,
    runtime: tokio::runtime::Handle,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    // Warm the host token/SID before the first client can connect: a host
    // that cannot verify itself must not serve at all.
    let security = UserOnlySecurity::new()
        .map_err(|error| anyhow::anyhow!("building the current-user pipe DACL failed: {error}"))?;
    host_token_user()?;

    let full_name = format!(r"\\.\pipe\{name}");
    let wide_name = wide(&full_name);
    eprintln!("host: serving named pipe {full_name}");

    let live: Arc<LiveStreams> = Arc::new(LiveStreams::new());
    let mut first_instance = true;
    loop {
        let instance = create_pipe_instance(&wide_name, &security, first_instance)?;
        first_instance = false;
        let raw = instance.as_raw_handle() as HANDLE;
        let connected = unsafe { ConnectNamedPipe(raw, std::ptr::null_mut()) };
        if connected == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                // `instance` (OwnedHandle) closes itself on the way out.
                anyhow::bail!("ConnectNamedPipe failed: {error}");
            }
        }
        if stop.load(Ordering::SeqCst) {
            // The stop poke woke the parked instance; stop cleanly instead
            // of serving the throwaway wake connection.
            drop(instance);
            break;
        }
        if !verify_pipe_peer(raw) {
            eprintln!("host: rejected unverifiable pipe peer");
            // Disconnect is a borrowed use of the handle; the close itself
            // happens exactly once, when the OwnedHandle drops. There is no
            // raw CloseHandle anywhere on this path.
            unsafe { DisconnectNamedPipe(raw) };
            drop(instance);
            continue;
        }
        if live.len() >= MAX_CONNECTIONS {
            eprintln!("host: connection cap reached; refusing");
            unsafe { DisconnectNamedPipe(raw) };
            drop(instance);
            continue;
        }
        // Hand the pipe handle to std's Read+Write world for shared framing.
        let stream = std::fs::File::from(instance);
        let (router, guard) = match open_connection_plane(&plane, read_only) {
            Ok(connection) => connection,
            Err(error) => {
                // Controlled refusal at the session cap: one structured
                // error frame, then close. The accept loop lives on.
                eprintln!("host: refusing connection; session grant unavailable: {error}");
                let mut writer = &stream;
                let _ = write_frame(&mut writer, &session_unavailable_frame(&plane.profile));
                // Close (not DisconnectNamedPipe) so the client can read
                // the refusal bytes before seeing EOF.
                drop(stream);
                continue;
            }
        };
        // Split the pipe into a reader (owned by the request loop) and a
        // duplicated writer shared by the response path and the event
        // forwarder under one whole-frame lock. The cancel hook covers both
        // handle halves, so shutdown and the forwarder's close path
        // interrupt every pending I/O this connection can be parked in.
        let write_half = match stream.try_clone() {
            Ok(write_half) => write_half,
            Err(error) => {
                eprintln!("host: dropping connection; write half unavailable: {error}");
                drop(guard);
                continue;
            }
        };
        let cancel_write = match stream.try_clone() {
            Ok(cancel_write) => cancel_write,
            Err(error) => {
                eprintln!("host: dropping connection; cancel half unavailable: {error}");
                drop(guard);
                continue;
            }
        };
        let shared = Arc::new(stream);
        // The interrupt flag rides the cancel hook: every teardown path
        // (worker exit, forwarder close, shutdown drain) goes through it, so
        // the peek-poll request loop below always learns about teardown.
        let reader_interrupt = Arc::new(AtomicBool::new(false));
        let hook_interrupt = Arc::clone(&reader_interrupt);
        let hang_up: CancelHook = {
            let read = Arc::clone(&shared);
            Arc::new(move || {
                hook_interrupt.store(true, Ordering::Relaxed);
                unsafe {
                    CancelIoEx(read.as_raw_handle() as HANDLE, std::ptr::null());
                    CancelIoEx(cancel_write.as_raw_handle() as HANDLE, std::ptr::null());
                }
            })
        };
        let events = ConnectionEvents::new(write_half, plane.profile.clone(), hang_up);
        let id = live.insert(events.cancel_hook());
        let live_in_worker = Arc::clone(&live);
        let runtime = runtime.clone();
        std::thread::spawn(move || {
            let mut events = events;
            let mut reader: &std::fs::File = shared.as_ref();
            // Non-overlapped I/O on one named-pipe instance serializes across
            // ALL handles of the file object (the duplicated write half
            // included): a request loop parked in ReadFile blocks the event
            // forwarder's notifications until the client happens to send
            // another request. While a subscription is live the loop therefore
            // polls PeekNamedPipe — no pending I/O while waiting — and only
            // commits to a blocking read once bytes are actually there.
            // Without a forwarder there is nothing to starve, so the loop
            // uses the plain blocking read, whose prompt disconnect (EOF /
            // ERROR_BROKEN_PIPE) releases the connection slot immediately.
            let read_next_frame = move |reader: &mut &std::fs::File, may_block: bool| {
                if may_block {
                    return super::read_frame(reader);
                }
                loop {
                    if reader_interrupt.load(Ordering::Relaxed) {
                        // Teardown asked this loop to stand down.
                        return Ok(None);
                    }
                    let mut available = 0u32;
                    let peeked = unsafe {
                        PeekNamedPipe(
                            reader.as_raw_handle() as HANDLE,
                            std::ptr::null_mut(),
                            0,
                            std::ptr::null_mut(),
                            &mut available,
                            std::ptr::null_mut(),
                        )
                    };
                    if peeked == 0 {
                        let error = std::io::Error::last_os_error();
                        return match error.raw_os_error() {
                            // The client is gone: a clean disconnect, not a
                            // fault. The worker unwinds and revokes its grant.
                            Some(code)
                                if code == ERROR_BROKEN_PIPE as i32
                                    || code == ERROR_NO_DATA as i32 =>
                            {
                                Ok(None)
                            }
                            _ => Err(error),
                        };
                    }
                    if available > 0 {
                        // Bytes are here; the frame read below can only park
                        // for the remainder of an in-flight frame, never for
                        // an idle connection.
                        return super::read_frame(reader);
                    }
                    std::thread::sleep(REQUEST_POLL);
                }
            };
            let _ = process_connection(
                &mut reader,
                &mut events,
                &router,
                &runtime,
                &read_next_frame,
            );
            // The connection is over: stop the event forwarder, interrupt
            // anything still parked (a forwarder wedged writing to a silent
            // consumer included), then release the grant and the live entry.
            events.shutdown();
            drop(guard);
            live_in_worker.remove(id);
        });
    }
    // Bounded wind-down: settle what is in flight within the grace, then
    // interrupt the still-blocked synchronous reads/writes. CancelIoEx
    // cancels pending I/O on the handle regardless of the issuing thread,
    // which is the bounded interrupt for a synchronous Connect/Read here.
    live.drain(SHUTDOWN_GRACE);
    Ok(())
}
