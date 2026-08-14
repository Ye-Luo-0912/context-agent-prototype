use grep_cooperative_cancel::{scan, Cancel};

fn file(name: &str, needle_every: usize, lines: usize) -> (String, String) {
    let mut body = String::new();
    for i in 1..=lines {
        if i % needle_every == 0 {
            body.push_str("needle here\n");
        } else {
            body.push_str("padding line without the token\n");
        }
    }
    (name.into(), body)
}

#[test]
fn cancel_keeps_hits_already_found() {
    let files = vec![
        file("a.txt", 4, 24),
        file("b.txt", 4, 24),
        file("c.txt", 4, 24),
    ];
    let cancel = Cancel::trip_after(2);
    let (hits, cancelled) = scan(&files, "needle", &cancel);
    assert!(cancelled, "token must stop the walk");
    assert!(!hits.is_empty(), "hits collected before cancel must remain");
    assert!(
        hits.iter().all(|hit| hit.file == "a.txt"),
        "later files must not be scanned after cancel: {hits:?}"
    );
}

#[test]
fn complete_scan_when_token_never_trips() {
    let files = vec![file("only.txt", 3, 12)];
    let cancel = Cancel::trip_after(10_000);
    let (hits, cancelled) = scan(&files, "needle", &cancel);
    assert!(!cancelled);
    assert_eq!(hits.len(), 4);
    assert_eq!(hits[0].line, 3);
}
