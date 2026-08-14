//! 有界压缩：B 的滚动折叠和 C 的派生蒸馏共用同一算子。
//!
//! 源和输出都有硬上限。B 用结果替换被折叠的历史；C 把结果写成带
//! `DerivedFrom` 的派生项，原文仍可检索。实现可以是脚本化（CI）或
//! 模型调用（live）；trait 本身不依赖任何 provider。

use async_trait::async_trait;

use crate::error::AgentResult;

/// 交给压缩器的源文本上限（字符）。折叠再多也不能让摘要输入随历史膨胀。
pub const COMPACTION_SOURCE_CHARS: usize = 2_000;
/// 压缩结果上限（字符）。模型若写超，调用方截断后再入引擎。
pub const COMPACTION_OUTPUT_CHARS: usize = 512;

/// 一次有界压缩请求。`source` 应由调用方先经过 [`bound_compaction_source`]。
#[derive(Debug, Clone)]
pub struct CompactionRequest {
    /// 被折叠或被蒸馏的条目数（解释用，不参与预算）。
    pub folded_items: usize,
    pub source: String,
}

/// 一次有界压缩结果。token 字段是压缩调用本身的花费，不是工作集可见体积。
#[derive(Debug, Clone, Default)]
pub struct CompactionOutput {
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// 把任意长的折叠正文收成压缩器输入。
pub fn bound_compaction_source(raw: &str) -> String {
    bound_chars(raw, COMPACTION_SOURCE_CHARS)
}

/// 把压缩器输出收进硬上限；空串保持空串。
pub fn bound_compaction_output(raw: &str) -> String {
    bound_chars(raw.trim(), COMPACTION_OUTPUT_CHARS)
}

fn bound_chars(raw: &str, cap: usize) -> String {
    if raw.chars().count() <= cap {
        return raw.to_string();
    }
    raw.chars().take(cap).collect()
}

/// B 与 C 共用的有界压缩算子。
#[async_trait]
pub trait BoundedCompactor: Send + Sync {
    async fn compact(&self, request: CompactionRequest) -> AgentResult<CompactionOutput>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_and_output_caps_are_hard() {
        let long: String = "a".repeat(COMPACTION_SOURCE_CHARS + 50);
        let source = bound_compaction_source(&long);
        assert_eq!(source.chars().count(), COMPACTION_SOURCE_CHARS);

        let long_out: String = "b".repeat(COMPACTION_OUTPUT_CHARS + 20);
        let output = bound_compaction_output(&long_out);
        assert_eq!(output.chars().count(), COMPACTION_OUTPUT_CHARS);
        assert_eq!(bound_compaction_output("  note  "), "note");
        assert_eq!(bound_compaction_output("   "), "");
    }
}
