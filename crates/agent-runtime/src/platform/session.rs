//! 连接级 operation-control 会话安装。
//!
//! 授权在握手时由可信组合根写入；适配器绑定该会话后覆盖信封上的
//! `authority_ref`。对等端自报的字符串不能提权，撤销立即生效。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_contracts::{AgentError, AgentResult, OperationId, RunId};
use agent_platform_protocol::{
    JsonDecodeBudget, NegotiatedContractProfile, OperationCancelRequest, OperationQueryRequest,
    PlatformEnvelope, PlatformResponse, Route, decode_value,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{
    OperationControlAction, OperationControlAuthorization, OperationControlAuthorizationRequest,
    OperationControlAuthorizer, OperationControlRouter,
};
use crate::RuntimeHandle;

/// 同时存活的已安装会话上限。超出则拒绝新安装，避免连接表膨胀。
pub const MAX_OPERATION_CONTROL_SESSIONS: usize = 64;
/// 单帧 operation-control 信封上限。query/cancel 正文为空，16 KiB 足够身份字段。
pub const MAX_OPERATION_CONTROL_ENVELOPE_BYTES: usize = 16 * 1024;

/// 一次已认证连接上允许的 operation-control 动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationControlGrant {
    pub allow_query: bool,
    pub allow_cancel: bool,
    pub allow_observe_accepted: bool,
}

impl OperationControlGrant {
    pub const fn query_only() -> Self {
        Self {
            allow_query: true,
            allow_cancel: false,
            allow_observe_accepted: true,
        }
    }

    pub const fn operator() -> Self {
        Self {
            allow_query: true,
            allow_cancel: true,
            allow_observe_accepted: true,
        }
    }

    fn allows(self, action: OperationControlAction) -> bool {
        match action {
            OperationControlAction::Query => self.allow_query,
            OperationControlAction::Cancel => self.allow_cancel,
            OperationControlAction::ObserveAccepted => self.allow_observe_accepted,
        }
    }
}

/// 可信组合根持有的会话表。对等端不能往这里插入记录。
pub struct OperationControlSessionRegistry {
    run_id: RunId,
    sessions: Mutex<HashMap<String, OperationControlGrant>>,
}

impl OperationControlSessionRegistry {
    pub fn new(run_id: RunId) -> Arc<Self> {
        Arc::new(Self {
            run_id,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    /// 安装一个不可猜测的会话并返回其 id。
    pub fn install(&self, grant: OperationControlGrant) -> AgentResult<String> {
        let mut sessions = self.sessions.lock().expect("session registry poisoned");
        if sessions.len() >= MAX_OPERATION_CONTROL_SESSIONS {
            return Err(AgentError::InvalidRequest(format!(
                "operation-control session registry is limited to {MAX_OPERATION_CONTROL_SESSIONS} live grants"
            )));
        }
        let session_id = OperationId::new().to_string();
        sessions.insert(session_id.clone(), grant);
        Ok(session_id)
    }

    pub fn revoke(&self, session_id: &str) -> AgentResult<()> {
        let mut sessions = self.sessions.lock().expect("session registry poisoned");
        if sessions.remove(session_id).is_none() {
            return Err(AgentError::InvalidRequest(
                "operation-control session is not installed".into(),
            ));
        }
        Ok(())
    }

    /// 把已安装会话绑到一条连接。之后授权查这个 id，不查信封。
    pub fn bind(self: &Arc<Self>, session_id: &str) -> AgentResult<BoundSessionAuthorizer> {
        if session_id.is_empty() || session_id.len() > 256 {
            return Err(AgentError::InvalidRequest(
                "operation-control session id is out of bounds".into(),
            ));
        }
        {
            let sessions = self.sessions.lock().expect("session registry poisoned");
            if !sessions.contains_key(session_id) {
                return Err(AgentError::InvalidRequest(
                    "operation-control session is not installed".into(),
                ));
            }
        }
        Ok(BoundSessionAuthorizer {
            run_id: self.run_id,
            session_id: session_id.to_owned(),
            registry: Arc::clone(self),
        })
    }
}

/// 一条连接上的授权器：忽略 wire `authority_ref`，只看安装表。
pub struct BoundSessionAuthorizer {
    run_id: RunId,
    session_id: String,
    registry: Arc<OperationControlSessionRegistry>,
}

impl BoundSessionAuthorizer {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl OperationControlAuthorizer for BoundSessionAuthorizer {
    fn authorize(
        &self,
        request: &OperationControlAuthorizationRequest,
    ) -> OperationControlAuthorization {
        if request.run_id != self.run_id {
            return OperationControlAuthorization::Denied;
        }
        let sessions = self
            .registry
            .sessions
            .lock()
            .expect("session registry poisoned");
        match sessions.get(&self.session_id) {
            Some(grant) if grant.allows(request.action) => {
                OperationControlAuthorization::Authorized
            }
            _ => OperationControlAuthorization::Denied,
        }
    }
}

/// 已认证的 in-process operation-control 适配器。
///
/// 它不拥有 socket 或进程：调用方用 `agent_process::FramedProtocolSession`
/// （或同等的 `read_frame`）读完一帧后把正文交进来。对等端的
/// `authority_ref` 会被连接会话覆盖后再进 router。
/// 本地传输身份本身不是 Core 授权。
pub struct AuthenticatedOperationControlAdapter {
    router: OperationControlRouter,
    session_id: String,
    max_envelope_bytes: usize,
}

impl AuthenticatedOperationControlAdapter {
    pub fn bind(
        profile: NegotiatedContractProfile,
        runtime: RuntimeHandle,
        registry: Arc<OperationControlSessionRegistry>,
        session_id: &str,
    ) -> AgentResult<Self> {
        Self::bind_with_limit(
            profile,
            runtime,
            registry,
            session_id,
            MAX_OPERATION_CONTROL_ENVELOPE_BYTES,
        )
    }

    pub fn bind_with_limit(
        profile: NegotiatedContractProfile,
        runtime: RuntimeHandle,
        registry: Arc<OperationControlSessionRegistry>,
        session_id: &str,
        max_envelope_bytes: usize,
    ) -> AgentResult<Self> {
        if max_envelope_bytes == 0 {
            return Err(AgentError::InvalidRequest(
                "operation-control envelope bound must be positive".into(),
            ));
        }
        let authorizer = Arc::new(registry.bind(session_id)?);
        Ok(Self {
            session_id: authorizer.session_id().to_owned(),
            router: OperationControlRouter::new(profile, runtime, authorizer)?,
            max_envelope_bytes,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn router(&self) -> &OperationControlRouter {
        &self.router
    }

    /// 处理一帧 JSON 信封（不含传输换行）。超长或无法解析时不进入 actor。
    pub async fn handle_frame(&self, bytes: &[u8]) -> AgentResult<Vec<u8>> {
        if bytes.len() > self.max_envelope_bytes {
            return Err(AgentError::InvalidRequest(format!(
                "operation-control envelope is {} bytes, above the {} byte bound",
                bytes.len(),
                self.max_envelope_bytes
            )));
        }
        // 控制面预算在 typed 投影之前挡住 DOM 放大；对等端不能靠塞空对象绕过 16 KiB 帧帽。
        let value = decode_value(bytes, &JsonDecodeBudget::control_plane()).map_err(|error| {
            AgentError::InvalidRequest(format!("operation-control envelope is malformed: {error}"))
        })?;
        let peek: RoutePeek = typed_from_value(&value, "operation-control envelope")?;
        if peek.route.is_operation_query() {
            let mut request: PlatformEnvelope<OperationQueryRequest> =
                typed_from_value(&value, "operation query envelope")?;
            stamp_connection_authority(&mut request, &self.session_id);
            let response = self.router.query(request).await?;
            encode_envelope(&response)
        } else if peek.route.is_operation_cancel() {
            let mut request: PlatformEnvelope<OperationCancelRequest> =
                typed_from_value(&value, "operation cancel envelope")?;
            stamp_connection_authority(&mut request, &self.session_id);
            let response = self.router.cancel(request).await?;
            encode_envelope(&response)
        } else {
            Err(AgentError::InvalidRequest(format!(
                "operation-control adapter does not serve {}/{}",
                peek.route.namespace, peek.route.operation
            )))
        }
    }
}

#[derive(Debug, Deserialize)]
struct RoutePeek {
    route: Route,
}

/// 从已有界的 Value 投影类型，避免对同一帧再解码一次。
fn typed_from_value<T: DeserializeOwned>(
    value: &serde_json::Value,
    what: &'static str,
) -> AgentResult<T> {
    T::deserialize(value)
        .map_err(|error| AgentError::InvalidRequest(format!("{what} is malformed: {error}")))
}

fn stamp_connection_authority<P>(envelope: &mut PlatformEnvelope<P>, session_id: &str) {
    if let Some(work) = envelope.work.as_mut() {
        work.authority_ref = Some(session_id.to_owned());
    }
}

fn encode_envelope<P: serde::Serialize>(
    envelope: &PlatformEnvelope<PlatformResponse<P>>,
) -> AgentResult<Vec<u8>> {
    serde_json::to_vec(envelope).map_err(|error| {
        AgentError::Internal(format!("serialize operation-control response: {error}"))
    })
}
