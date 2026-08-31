//! 测试专用假协调器宿主：按 `mode` 参数驱动四种协议违例/异常行为，
//! 供 `broker_host_coordinator` 的失败场景测试使用。不在任何产品路径
//! 中出现。
//!
//! - `silent`: 读取 stdin 但永不答复（stalled peer）。
//! - `stubborn`: 收到 shutdown 帧后既不答复也不退出。
//! - `oversize_reply`: 答复超限帧（客户端读侧 oversize）。
//! - `malformed_reply`: 答复非 JSON 文本。
//! - `partial_eof_reply`: 答复无换行终止符的 JSON 后退出。

use std::io::{Read, Write};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(mode) = args.next() else {
        eprintln!("usage: coordinator_fake <mode>");
        std::process::exit(2);
    };
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    // 读一条请求帧（有界、含换行），不验证内容。
    let mut line = Vec::new();
    let mut buffer = [0u8; 1];
    loop {
        match stdin.read(&mut buffer) {
            Ok(0) => break,
            Ok(_) => {
                line.push(buffer[0]);
                if line.ends_with(b"\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    match mode.as_str() {
        "silent" => {
            // 永远沉默：客户端 RPC 超时后 kill 会关闭管道。
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
        "stubborn" => {
            // 收到 shutdown 帧也不退出，检验 kill 兜底。
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
        "oversize_reply" => {
            let payload = vec![b'x'; 64 * 1024 + 1];
            let _ = stdout.write_all(&payload);
            let _ = stdout.write_all(b"\n");
            let _ = stdout.flush();
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
        "malformed_reply" => {
            let _ = stdout.write_all(b"this is not json\n");
            let _ = stdout.flush();
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
        "partial_eof_reply" => {
            // 无换行终止符即退出；客户端必须把它当 PartiaEof 违规。
            let _ = stdout.write_all(br#"{"ok":true}"#);
            let _ = stdout.flush();
            drop(stdout);
            std::process::exit(0);
        }
        other => {
            eprintln!("coordinator_fake: unknown mode {other}");
            std::process::exit(2);
        }
    }
}
