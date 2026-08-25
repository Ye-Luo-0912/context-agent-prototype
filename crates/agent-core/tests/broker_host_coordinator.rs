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
    EffectAck, EffectBroker as _, EffectReservation, ProcessEffectBroker, ReservedEffect,
    ReservedRecord,
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
                applied: true,
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
                applied: false,
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
