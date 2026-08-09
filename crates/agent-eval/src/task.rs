//! The eval task: a no-tools constraint-retention workload.
//!
//! The provider endpoint used by the live harness (`pinaic`, model
//! `gpt-5.6-luna`) does not accept a `tools` array on the wire (any request
//! with tools fails upstream), so the live task cannot be a tool-using
//! coding task. It is instead a *context-retention* task, which is exactly
//! the dynamic working set's acceptance scenario: the first turn states
//! five constraints, several turns of unrelated high-volume noise follow,
//! and the final turn asks for the constraints. A model can only answer
//! from what its context frame retained — append-only keeps everything
//! (and pays for it), the dynamic engine must have kept the facts in view.

pub const CONSTRAINT_TURNS: usize = 20;

const FACTS: &str = "\
背景信息（请记住以下约束，稍后会提问）：\
1) 密码字段长度至少 24 字符；\
2) 所有时间戳使用 UTC；\
3) 禁止使用第三方库；\
4) 数据库使用 PostgreSQL；\
5) 日志级别默认 info。";

const NOISE: &str = "\
（与本任务无关的记录）茶水间新咖啡机型号为 Breville Barista Pro，\
豆仓容量 250 克，研磨档位 30 档，蒸汽棒可 360 度旋转，水箱 1.9 升，\
支持双杯同时萃取，清洗周期建议每 200 杯一次，保修期两年，\
周五下午清洁保养。";

const QUESTION: &str = "\
现在请回答两个问题（直接给出答案，不要解释）：\
Q1: 根据之前的讨论，密码字段的长度要求是多少？\
Q2: 数据库使用什么？";

/// The per-turn prompts of the task. Turn 0 states the facts, turns
/// 1..=6 are unrelated noise, the last turn asks the questions.
pub fn prompts() -> Vec<String> {
    let mut prompts = Vec::new();
    prompts.push(FACTS.to_string());
    for _ in 1..CONSTRAINT_TURNS - 1 {
        prompts.push(NOISE.to_string());
    }
    prompts.push(QUESTION.to_string());
    prompts
}

/// Acceptance: the final answer carries both facts.
pub fn verify(answer: &str) -> bool {
    let lower = answer.to_lowercase();
    let has_length = answer.contains("24");
    let has_db = lower.contains("postgres");
    has_length && has_db
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_shape_is_expected() {
        let prompts = prompts();
        assert_eq!(prompts.len(), CONSTRAINT_TURNS);
        assert!(prompts[0].contains("PostgreSQL"));
        assert!(prompts.last().unwrap().contains("Q1"));
    }

    #[test]
    fn verify_accepts_correct_answers_and_rejects_wrong_ones() {
        assert!(verify("密码字段 24 字符，数据库 PostgreSQL。"));
        assert!(verify("24 characters, PostgreSQL"));
        assert!(!verify("密码字段 8 字符，数据库 MySQL。"));
        assert!(!verify("不确定"));
        assert!(!verify(""));
    }
}
