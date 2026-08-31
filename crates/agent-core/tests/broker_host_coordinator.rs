//! 进程外协调器端到端夹具：真实 `broker_host` 子进程 + 客户端。
//! 账本必须独立于任何一方存活：客户端重连、宿主被强杀之后的分类
//! 都由持久日志决定，而不是任何一侧的内存状态。

use std::{
    io::{BufRead as _, BufReader, Write as _},
    path::Path,
    process::{Command, Stdio},
};

use agent_contracts::{
    AgentResult, ArgumentDigest, EffectDurability, EffectId, EffectReceipt, EffectReconciliation,
    OperationEffectContext, OperationId, RunId, ToolOperationIdentity, TurnId,
};
use agent_core::{
    CoordinatorRequest, EffectAck, EffectBroker as _, EffectReservation, ProcessEffectBroker,
    ReservedEffect, ReservedRecord,
};

fn identity(operation_id: OperationId) -> ToolOperationIdentity {
    ToolOperationIdentity {
        run_id: RunId::new(),
        task_id: None,
        turn_id: TurnId::new(),
        scope_id: None,
        operation_id,
        generation: 7,
        call_id: "call-coord".into(),
        tool_name: "cap.remote".into(),
        argument_digest: ArgumentDigest::sha256_bytes(b"coord"),
    }
}

fn context_of(identity: &ToolOperationIdentity, effect_id: EffectId) -> OperationEffectContext {
    OperationEffectContext {
        identity: identity.clone(),
        effect_id,
    }
}

fn reservation_for(identity: &ToolOperationIdentity, effect_id: EffectId) -> EffectReservation {
    EffectReservation {
        run_id: identity.run_id,
        operation_id: identity.operation_id,
        effect_id,
        argument_digest: identity.argument_digest,
        generation: identity.generation,
        intent: None,
    }
}

/// 本地执行的占位效果体：协调器只记账，应用永远发生在请求方。
struct LocalEffect {
    applied: bool,
}

#[async_trait::async_trait]
impl agent_contracts::Effect for LocalEffect {
    fn describe(&self) -> String {
        "local coordinator fixture".into()
    }

    async fn commit(self: Box<Self>) -> EffectReceipt {
        if self.applied {
            EffectReceipt::Applied {
                durability: EffectDurability::Durable,
                evidence: None,
            }
        } else {
            EffectReceipt::NotApplied {
                error: "fixture refusal".into(),
            }
        }
    }

    async fn rollback(self: Box<Self>, _reason: &str) -> AgentResult<()> {
        Ok(())
    }
}

#[tokio::test]
async fn ledger_classes_survive_client_reconnects() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("reservations.jsonl");
    let program = env!("CARGO_BIN_EXE_broker_host");

    let identity = identity(OperationId::new());
    let applied = EffectId::new();
    let refused = EffectId::new();

    {
        let coordinator = ProcessEffectBroker::connect(Path::new(program), &journal).unwrap();
        let reservation = coordinator
            .reserve(reservation_for(&identity, applied))
            .await
            .unwrap();
        assert!(reservation.starts_with("coord/"), "{reservation}");
        assert!(matches!(
            coordinator
                .reconcile_reservation(&context_of(&identity, applied))
                .unwrap(),
            Some(EffectReconciliation::NotApplied { .. })
        ));

        // 派发在本地应用；账本先记意图后记结果。
        let receipt = coordinator
            .dispatch(ReservedEffect {
                reservation: reservation_for(&identity, applied),
                reservation_id: reservation.clone(),
                effect: Box::new(LocalEffect { applied: true }),
            })
            .await;
        assert!(matches!(receipt, EffectReceipt::Applied { .. }));
        coordinator
            .ack(EffectAck {
                reservation_id: reservation,
                operation_id: identity.operation_id,
                settlement: agent_contracts::EffectAckSettlement::Applied {
                    durability: agent_contracts::EffectDurability::Durable,
                },
                receipt_summary: "fixture".into(),
            })
            .await
            .unwrap();
        assert!(matches!(
            coordinator
                .reconcile_reservation(&context_of(&identity, applied))
                .unwrap(),
            Some(EffectReconciliation::Applied { .. })
        ));

        // 派发了但效果拒绝应用、应答 false：NotApplied 而非 Applied。
        let reservation = coordinator
            .reserve(reservation_for(&identity, refused))
            .await
            .unwrap();
        let receipt = coordinator
            .dispatch(ReservedEffect {
                reservation: reservation_for(&identity, refused),
                reservation_id: reservation.clone(),
                effect: Box::new(LocalEffect { applied: false }),
            })
            .await;
        assert!(matches!(receipt, EffectReceipt::NotApplied { .. }));
        coordinator
            .ack(EffectAck {
                reservation_id: reservation,
                operation_id: identity.operation_id,
                settlement: agent_contracts::EffectAckSettlement::NotApplied,
                receipt_summary: "fixture".into(),
            })
            .await
            .unwrap();
    }

    // 重连同一日志：分类原样可查——账本不属于任何一个客户端。
    let coordinator = ProcessEffectBroker::connect(Path::new(program), &journal).unwrap();
    assert!(matches!(
        coordinator
            .reconcile_reservation(&context_of(&identity, applied))
            .unwrap(),
        Some(EffectReconciliation::Applied { .. })
    ));
    assert!(matches!(
        coordinator
            .reconcile_reservation(&context_of(&identity, refused))
            .unwrap(),
        Some(EffectReconciliation::NotApplied { .. })
    ));
}

/// 宿主被强杀（kill -9 等价）之后，已记 dispatched 未应答的预留
/// 重开后必须是 Ambiguous——账本比进程活得久。
#[tokio::test]
async fn killed_host_leaves_an_ambiguous_dispatched_window() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("reservations.jsonl");
    let program = env!("CARGO_BIN_EXE_broker_host");
    let identity = identity(OperationId::new());
    let effect_id = EffectId::new();

    // 直接驱动协议：预约并记 dispatched，然后强杀宿主，不给它任何
    // 优雅收尾的机会。
    let mut child = Command::new(program)
        .arg(&journal)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());

        let record = ReservedRecord {
            reservation_id: String::new(),
            run_id: identity.run_id,
            operation_id: identity.operation_id,
            argument_digest: identity.argument_digest,
            generation: identity.generation,
            intent: None,
        };
        let reserve_line = format!(
            "{{\"op\":\"reserve\",\"effect_id\":{},\"record\":{}}}\n",
            serde_json::to_string(&effect_id).unwrap(),
            serde_json::to_string(&record).unwrap()
        );
        stdin.write_all(reserve_line.as_bytes()).unwrap();
        let mut reply = String::new();
        stdout.read_line(&mut reply).unwrap();
        let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["ok"], true);
        let reservation_id = reply["reservation_id"].as_str().unwrap().to_string();

        let dispatched_line = format!(
            "{{\"op\":\"dispatched\",\"effect_id\":{},\"reservation_id\":\"{}\"}}\n",
            serde_json::to_string(&effect_id).unwrap(),
            reservation_id
        );
        stdin.write_all(dispatched_line.as_bytes()).unwrap();
        let mut reply = String::new();
        stdout.read_line(&mut reply).unwrap();
        assert!(reply.contains("\"ok\":true"), "{reply}");
    }
    child.kill().unwrap();
    child.wait().unwrap();

    // 官方客户端重开同一日志：该窗口只能按 Ambiguous 处理。
    let coordinator = ProcessEffectBroker::connect(Path::new(program), &journal).unwrap();
    assert!(matches!(
        coordinator
            .reconcile_reservation(&context_of(&identity, effect_id))
            .unwrap(),
        Some(EffectReconciliation::Ambiguous { .. })
    ));
}

// ---------------------------------------------------------------------------
// 失败场景：服务端严格帧语义（进程内 duplex）+ 客户端有界/超时/毒化
// （测试专用假宿主子进程）。全部 fail closed。
// ---------------------------------------------------------------------------

fn fake_program() -> &'static str {
    env!("CARGO_BIN_EXE_coordinator_fake")
}

fn reserve_request(identity: &ToolOperationIdentity, effect_id: EffectId) -> CoordinatorRequest {
    CoordinatorRequest::Reserve {
        effect_id,
        record: Box::new(ReservedRecord::from_reservation(
            String::new(),
            &reservation_for(identity, effect_id),
        )),
    }
}

async fn read_frame_from(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut frame = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = reader.read(&mut byte).await.unwrap();
        if read == 0 {
            return frame;
        }
        frame.push(byte[0]);
        if byte[0] == b'\n' {
            return frame;
        }
    }
}

/// 进程内帧服务器：`serve_broker_frames` 跑在 duplex 服务端，返回
/// 服务端任务与客户端 stream（测试直接驱动客户端侧）。
async fn spawn_frame_server(
    dir: &tempfile::TempDir,
) -> (
    tokio::task::JoinHandle<agent_contracts::AgentResult<()>>,
    tokio::io::DuplexStream,
) {
    let journal_path = dir.path().join("j.jsonl");
    let journal = agent_core::ReservationJournal::open(journal_path.as_path()).unwrap();
    let (duplex, client) = tokio::io::duplex(64 * 1024);
    let (read_half, write_half) = tokio::io::split(duplex);
    let task = tokio::spawn(async move {
        let mut server_read = tokio::io::BufReader::new(read_half);
        let mut server_write = write_half;
        agent_core::serve_broker_frames(&mut server_read, &mut server_write, &journal).await
    });
    (task, client)
}

#[tokio::test]
async fn server_rejects_oversize_request() {
    use tokio::io::AsyncWriteExt;

    let dir = tempfile::tempdir().unwrap();
    let (task, mut client) = spawn_frame_server(&dir).await;
    let _ = client
        .write_all(&vec![b'x'; agent_core::MAX_COORDINATOR_LINE_BYTES + 1])
        .await;
    let _ = client.write_all(b"\n").await;
    let _ = client.flush().await;
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("server must terminate on an oversize frame")
        .unwrap()
        .expect_err("oversize request must fail closed");
    assert!(
        error.to_string().contains("bound"),
        "server error must mention the bound: {error}"
    );
}

#[tokio::test]
async fn server_rejects_empty_and_partial_eof_frames() {
    use tokio::io::AsyncWriteExt;

    let dir = tempfile::tempdir().unwrap();
    let (task, mut client) = spawn_frame_server(&dir).await;
    // 空帧：严格帧语义下是协议违规，不再是「跳过」。
    let _ = client.write_all(b"\n").await;
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("server must terminate on an empty frame")
        .unwrap()
        .expect_err("empty frame must fail closed");
    assert!(error.to_string().contains("empty"), "{error}");
}

#[tokio::test]
async fn server_rejects_partial_eof_frame() {
    use tokio::io::AsyncWriteExt;

    let dir = tempfile::tempdir().unwrap();
    let (task, mut client) = spawn_frame_server(&dir).await;
    // 有字节、无换行终止符的 EOF：之前会被 `read_line` 静默当成完整行。
    let _ = client.write_all(br#"{"op":"shutdown"}"#).await;
    let _ = client.flush().await;
    drop(client);
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("server must terminate on a partial EOF frame")
        .unwrap()
        .expect_err("partial EOF must fail closed");
    assert!(
        error.to_string().contains("mid-frame"),
        "server error must call out the mid-frame EOF: {error}"
    );
}

#[tokio::test]
async fn server_rejects_malformed_json() {
    use tokio::io::AsyncWriteExt;

    let dir = tempfile::tempdir().unwrap();
    let (task, mut client) = spawn_frame_server(&dir).await;
    let _ = client.write_all(b"this is not json\n").await;
    let _ = client.flush().await;
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("server must terminate on a malformed request")
        .unwrap()
        .expect_err("malformed JSON must fail closed, not reply-and-continue");
    assert!(error.to_string().contains("malformed"), "{error}");
}

#[tokio::test]
async fn server_roundtrips_reserve_then_shutdown_and_clean_eof() {
    use tokio::io::AsyncWriteExt;

    let dir = tempfile::tempdir().unwrap();
    let (task, mut client) = spawn_frame_server(&dir).await;
    let identity = identity(OperationId::new());
    let effect_id = EffectId::new();
    let encoded = serde_json::to_vec(&reserve_request(&identity, effect_id)).unwrap();
    client.write_all(&encoded).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    client.flush().await.unwrap();
    let reply_frame = read_frame_from(&mut client).await;
    let reply: agent_core::CoordinatorReply =
        serde_json::from_str(std::str::from_utf8(&reply_frame).unwrap()).unwrap();
    assert!(reply.ok, "reserve must be accepted: {reply:?}");
    assert!(reply.reservation_id.is_some());
    // shutdown 帧：宿主干净返回。
    client.write_all(br#"{"op":"shutdown"}"#).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    client.flush().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("shutdown must end the session")
        .unwrap()
        .expect("clean shutdown is not an error");
}

#[tokio::test]
async fn stalled_peer_rpc_times_out_and_poisons() {
    let timeouts = agent_core::CoordinatorTimeouts {
        rpc: std::time::Duration::from_millis(300),
        ..Default::default()
    };
    let coordinator = ProcessEffectBroker::connect_with_timeouts(
        Path::new(fake_program()),
        // 假宿主把唯一参数当行为模式；真实 journal 从未被它打开。
        Path::new("silent"),
        timeouts,
    )
    .unwrap();
    let identity = identity(OperationId::new());
    let error = coordinator
        .reserve(reservation_for(&identity, EffectId::new()))
        .await
        .expect_err("a silent host must time out");
    assert!(error.to_string().contains("timed out"), "{error}");
    let again = coordinator
        .reserve(reservation_for(&identity, EffectId::new()))
        .await
        .expect_err("a poisoned connection must stay fail-closed");
    assert!(again.to_string().contains("poisoned"), "{again}");
    coordinator.shutdown();
}

#[tokio::test]
async fn oversize_reply_poisons_the_client() {
    let timeouts = agent_core::CoordinatorTimeouts {
        rpc: std::time::Duration::from_secs(2),
        ..Default::default()
    };
    let coordinator = ProcessEffectBroker::connect_with_timeouts(
        Path::new(fake_program()),
        Path::new("oversize_reply"),
        timeouts,
    )
    .unwrap();
    let identity = identity(OperationId::new());
    let error = coordinator
        .reserve(reservation_for(&identity, EffectId::new()))
        .await
        .expect_err("an oversize reply must fail closed");
    assert!(error.to_string().contains("bound"), "{error}");
    coordinator.shutdown();
}

#[tokio::test]
async fn partial_eof_reply_fails_closed() {
    let timeouts = agent_core::CoordinatorTimeouts {
        rpc: std::time::Duration::from_secs(2),
        ..Default::default()
    };
    let coordinator = ProcessEffectBroker::connect_with_timeouts(
        Path::new(fake_program()),
        Path::new("partial_eof_reply"),
        timeouts,
    )
    .unwrap();
    let identity = identity(OperationId::new());
    let error = coordinator
        .reserve(reservation_for(&identity, EffectId::new()))
        .await
        .expect_err("a reply without a frame terminator must fail closed");
    assert!(error.to_string().contains("mid-frame"), "{error}");
    coordinator.shutdown();
}

#[tokio::test]
async fn stubborn_child_shutdown_is_bounded() {
    let timeouts = agent_core::CoordinatorTimeouts {
        rpc: std::time::Duration::from_millis(300),
        ..Default::default()
    };
    let coordinator = ProcessEffectBroker::connect_with_timeouts(
        Path::new(fake_program()),
        Path::new("stubborn"),
        timeouts,
    )
    .unwrap();
    // 假宿主对 shutdown 既不答复也不退出：shutdown 必须靠 kill 兜底
    // 并在有界时间内返回，绝不无限阻塞在 child.wait()。
    tokio::time::timeout(std::time::Duration::from_secs(3), async move {
        coordinator.shutdown()
    })
    .await
    .expect("shutdown must be bounded by kill + reap");
}

#[tokio::test]
async fn drop_does_not_block_on_a_stubborn_host() {
    let timeouts = agent_core::CoordinatorTimeouts {
        rpc: std::time::Duration::from_millis(300),
        ..Default::default()
    };
    let coordinator = ProcessEffectBroker::connect_with_timeouts(
        Path::new(fake_program()),
        Path::new("stubborn"),
        timeouts,
    )
    .unwrap();
    // 不先做任何 RPC：假宿主还在等第一条帧。Drop 必须立即返回（只
    // kill，不 wait）。spawn_blocking 断言同步 drop 不会卡住线程。
    let join = tokio::task::spawn_blocking(move || drop(coordinator));
    tokio::time::timeout(std::time::Duration::from_secs(3), join)
        .await
        .expect("Drop must not block on the child wait")
        .unwrap();
}
