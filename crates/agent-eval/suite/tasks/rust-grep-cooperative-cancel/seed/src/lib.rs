//! 种子：扫完所有文件才返回，不看取消标志。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub file: String,
    pub line: usize,
    pub text: String,
}

/// 测试替身：第 `trip_after` 次 `is_cancelled` 起变为 true。
#[derive(Clone)]
pub struct Cancel {
    flag: Arc<AtomicBool>,
    checks: Arc<AtomicUsize>,
    trip_after: usize,
}

impl Cancel {
    pub fn trip_after(checks: usize) -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            checks: Arc::new(AtomicUsize::new(0)),
            trip_after: checks,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        let n = self.checks.fetch_add(1, Ordering::SeqCst) + 1;
        if n >= self.trip_after {
            self.flag.store(true, Ordering::SeqCst);
        }
        self.flag.load(Ordering::SeqCst)
    }
}

/// 种子不查 cancel，总是扫完全部文件。
pub fn scan(files: &[(String, String)], needle: &str, _cancel: &Cancel) -> (Vec<Hit>, bool) {
    let mut hits = Vec::new();
    for (path, body) in files {
        for (index, line) in body.lines().enumerate() {
            if line.contains(needle) {
                hits.push(Hit {
                    file: path.clone(),
                    line: index + 1,
                    text: line.to_string(),
                });
            }
        }
    }
    (hits, false)
}
