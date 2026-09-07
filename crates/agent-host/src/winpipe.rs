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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, TokenUser, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::{build_connection_plane, process_connection, HostPlane, MAX_CONNECTIONS};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

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

fn create_pipe_instance(name: &[u16], security: &UserOnlySecurity) -> std::io::Result<OwnedHandle> {
    let attributes = security.attributes();
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
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
) -> anyhow::Result<()> {
    // Warm the host token/SID before the first client can connect: a host
    // that cannot verify itself must not serve at all.
    let security = UserOnlySecurity::new()
        .map_err(|error| anyhow::anyhow!("building the current-user pipe DACL failed: {error}"))?;
    host_token_user()?;

    let full_name = format!(r"\\.\pipe\{name}");
    let wide_name = wide(&full_name);
    eprintln!("host: serving named pipe {full_name}");

    let live = Arc::new(AtomicUsize::new(0));
    loop {
        let instance = create_pipe_instance(&wide_name, &security)?;
        let raw = instance.as_raw_handle() as HANDLE;
        let connected = unsafe { ConnectNamedPipe(raw, std::ptr::null_mut()) };
        if connected == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                anyhow::bail!("ConnectNamedPipe failed: {error}");
            }
        }
        if !verify_pipe_peer(raw) {
            eprintln!("host: rejected unverifiable pipe peer");
            unsafe {
                DisconnectNamedPipe(raw);
                CloseHandle(raw);
            }
            continue;
        }
        if live.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            eprintln!("host: connection cap reached; refusing");
            unsafe {
                DisconnectNamedPipe(raw);
                CloseHandle(raw);
            }
            continue;
        }
        live.fetch_add(1, Ordering::SeqCst);
        // Hand the pipe handle to std's Read+Write world for shared framing.
        let stream = std::fs::File::from(instance);
        let plane = build_connection_plane(&plane, read_only);
        let runtime = runtime.clone();
        let live = Arc::clone(&live);
        std::thread::spawn(move || {
            let mut stream = stream;
            let _ = process_connection(&mut stream, &plane, &runtime);
            drop(plane);
            live.fetch_sub(1, Ordering::SeqCst);
        });
    }
}
