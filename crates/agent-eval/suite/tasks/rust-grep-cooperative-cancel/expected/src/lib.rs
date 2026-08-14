//! TOOL-01 形状：文件之间以及文件内每 8 行查一次取消，已命中行保留。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub file: String,
    pub line: usize,
    pub text: String,
}

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

const CHECK_EVERY: usize = 8;

pub fn scan(files: &[(String, String)], needle: &str, cancel: &Cancel) -> (Vec<Hit>, bool) {
    let mut hits = Vec::new();
    for (path, body) in files {
        if cancel.is_cancelled() {
            return (hits, true);
        }
        for (index, line) in body.lines().enumerate() {
            if index > 0 && index % CHECK_EVERY == 0 && cancel.is_cancelled() {
                return (hits, true);
            }
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
