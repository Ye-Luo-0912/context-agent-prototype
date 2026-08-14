//! Typed runtime input envelope (`CTX-EVENT-01..03`).
//!
//! User input shares event/fence/consumption machinery with tools, but it is
//! not a `ToolOutput`. Only the user source may steer, interrupt, constrain,
//! or cancel. Tool or collaborator prose may propose evidence; it cannot
//! impersonate a user patch merely by sharing this envelope type.

use serde::{Deserialize, Serialize};

use crate::{RuntimeInputId, TaskId, TurnId};

/// 审计事件与 UI 只带这么长的预览；完整正文进证据平面（artifact）。
pub const USER_INPUT_PREVIEW_CHARS: usize = 240;
/// `write_artifact` / locator owner。与 `assistant-response` 并列。
pub const USER_INPUT_ARTIFACT_OWNER: &str = "user-input";
/// 周转中最多排队一条对话；再来的 UserMessage 记 Rejected。
pub const USER_INPUT_QUEUE_CAP: usize = 1;
/// 回放从 artifact 读用户正文的上限；超限 fail closed。
pub const USER_INPUT_REPLAY_MAX_BYTES: usize = 256 * 1024;

/// 输入从哪条权威路径进来。本地传输身份本身不是 grant。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputSource {
    #[default]
    User,
    Tool,
    Collaborator,
}

/// 这条输入能改什么。UserSteering 仅允许 `InputSource::User`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputAuthority {
    /// 可改目标/范围、打断、收回授权、覆盖计划。
    #[default]
    UserSteering,
    /// 证据/建议；不能冒充用户补丁。
    EvidenceOnly,
}

/// 第一版分类。取消与 `/focus` `/done` 仍走独立 `RuntimeCommand`，
/// 不经 `UserMessage` 解释；这些变体只是 taxonomy，避免以后把命令
/// 塞进对话字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    #[default]
    Dialogue,
    CancelTurn,
    CancelOperation,
    Command,
    PatchProposal,
}

/// `Received -> Interpreted -> Applied/Queued/Rejected -> Consumed -> Archived`。
/// ingest 成功后发 `Applied`。周转中第一条额外对话记 `Queued`（内存 1 槽，
/// 非崩溃耐久）；槽满或清理中记 `Rejected`。`/cancel` 在 `TurnCancelled`
/// 屏障之后补 `InterruptCommitted`（不经 UserMessage 解释）。模型消费 ack
/// 后发 `Consumed`，`TurnCompleted` 耐久屏障之后发 `Archived`（输入记录
/// 终态，不是 context GC）。`Received` / `Interpreted` 未接线。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputLifecycle {
    Received,
    Interpreted,
    #[default]
    Applied,
    Queued,
    Rejected,
    InterruptCommitted,
    Consumed,
    Archived,
}

/// 源授权的状态补丁。模型可以提议，Runtime 才提交。
/// 本切片对话路径的 proposal 恒为 `None`：不从自然语言推断权威补丁。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatePatchProposal {
    #[default]
    None,
}

/// 有界输入信封。`preview` 进事件日志；完整正文只存一次（artifact）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInputEnvelope {
    /// 有界预览。旧日志把全文放在 `content` 里，反序列化时别名接住。
    #[serde(alias = "content")]
    pub preview: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<RuntimeInputId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causal_parent: Option<RuntimeInputId>,
    #[serde(default)]
    pub source: InputSource,
    #[serde(default)]
    pub authority: InputAuthority,
    #[serde(default)]
    pub kind: InputKind,
    #[serde(default)]
    pub lifecycle: InputLifecycle,
    /// 密封 artifact。无 workspace 的测试路径可缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// 完整正文的字节数（不是预览长度）。
    #[serde(default)]
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "StatePatchProposal::is_none")]
    pub proposal: StatePatchProposal,
}

impl StatePatchProposal {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

impl RuntimeInputEnvelope {
    /// 短文本测试/回放助手：预览即全文（仍会按 cap 截断）。
    pub fn from_preview(body: impl Into<String>) -> Self {
        Self::user_dialogue(body, None, None, None, None, None)
    }

    /// 用户对话路径：source=User、authority=UserSteering、kind=Dialogue。
    pub fn user_dialogue(
        body: impl Into<String>,
        input_id: Option<RuntimeInputId>,
        task_id: Option<TaskId>,
        turn_id: Option<TurnId>,
        body_ref: Option<String>,
        digest: Option<String>,
    ) -> Self {
        let body = body.into();
        let bytes = body.len() as u64;
        Self {
            preview: bounded_preview(&body, USER_INPUT_PREVIEW_CHARS),
            input_id,
            task_id,
            turn_id,
            causal_parent: None,
            source: InputSource::User,
            authority: InputAuthority::UserSteering,
            kind: InputKind::Dialogue,
            lifecycle: InputLifecycle::Applied,
            body_ref,
            digest,
            bytes,
            proposal: StatePatchProposal::None,
        }
    }

    /// UserSteering 只能来自 User。其它组合 fail closed。
    pub fn validate(&self) -> Result<(), String> {
        if self.authority == InputAuthority::UserSteering && self.source != InputSource::User {
            return Err(
                "only the user source may carry UserSteering authority; tool/collaborator prose cannot impersonate a user patch".into(),
            );
        }
        if self.kind == InputKind::Dialogue && !self.proposal.is_none() {
            return Err("dialogue input cannot carry a state patch proposal".into());
        }
        if self.preview.chars().count() > USER_INPUT_PREVIEW_CHARS {
            return Err(format!(
                "input preview exceeds {USER_INPUT_PREVIEW_CHARS} chars"
            ));
        }
        Ok(())
    }

    /// 同一条记录换生命周期；id / preview / body_ref 不变。
    pub fn with_lifecycle(mut self, lifecycle: InputLifecycle) -> Self {
        self.lifecycle = lifecycle;
        self
    }

    /// 已成功 ingest 并入账的对话。缺省（旧日志）也算 Applied。
    pub fn is_applied(&self) -> bool {
        self.lifecycle == InputLifecycle::Applied
    }

    /// UI 把这条记成用户发言：Applied 或尚未开转的 Queued。
    pub fn appears_in_user_transcript(&self) -> bool {
        self.kind == InputKind::Dialogue
            && matches!(
                self.lifecycle,
                InputLifecycle::Applied | InputLifecycle::Queued
            )
    }

    /// 预览是否就是全文（无 body_ref 的旧日志、或未截断的短消息）。
    pub fn preview_covers_body(&self) -> bool {
        self.bytes == 0 || self.bytes == self.preview.len() as u64
    }
}

pub fn bounded_preview(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    // 预览总长不超过 cap：留下一格给省略号。
    let keep = max_chars.saturating_sub(1);
    let mut preview: String = text.chars().take(keep).collect();
    preview.push('…');
    preview
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_steering_rejects_tool_source() {
        let mut envelope = RuntimeInputEnvelope::from_preview("steer");
        envelope.source = InputSource::Tool;
        let error = envelope.validate().expect_err("tool cannot steer");
        assert!(error.contains("UserSteering"));
    }

    #[test]
    fn preview_is_capped() {
        let body = "x".repeat(USER_INPUT_PREVIEW_CHARS + 40);
        let envelope = RuntimeInputEnvelope::from_preview(&body);
        assert_eq!(envelope.bytes, body.len() as u64);
        assert_eq!(
            envelope.preview.chars().count(),
            USER_INPUT_PREVIEW_CHARS,
            "truncated preview stays inside the cap"
        );
        assert!(envelope.preview.ends_with('…'));
        envelope.validate().expect("capped preview is legal");
    }

    #[test]
    fn old_content_alias_deserializes_into_preview() {
        let raw = r#"{"type":"user_message_accepted","content":"fix AuthService.rs"}"#;
        let event: crate::RuntimeEvent = serde_json::from_str(raw).expect("legacy journal");
        let crate::RuntimeEvent::UserMessageAccepted { input } = event else {
            panic!("expected user_message_accepted");
        };
        assert_eq!(input.preview, "fix AuthService.rs");
        assert_eq!(input.kind, InputKind::Dialogue);
        assert_eq!(input.lifecycle, InputLifecycle::Applied);
    }

    #[test]
    fn new_preview_roundtrips_without_duplicating_content_key() {
        let event = crate::RuntimeEvent::user_message_accepted("hello");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"preview\":\"hello\""));
        assert!(
            !json.contains("\"content\""),
            "new journals must not emit the unbounded content key: {json}"
        );
        let parsed: crate::RuntimeEvent = serde_json::from_str(&json).unwrap();
        let crate::RuntimeEvent::UserMessageAccepted { input } = parsed else {
            panic!("roundtrip");
        };
        assert_eq!(input.preview, "hello");
    }

    #[test]
    fn rejected_dialogue_stays_user_steering_and_validates() {
        let envelope =
            RuntimeInputEnvelope::from_preview("second").with_lifecycle(InputLifecycle::Rejected);
        assert!(!envelope.is_applied());
        envelope
            .validate()
            .expect("rejected dialogue is still a legal record");
    }
}
