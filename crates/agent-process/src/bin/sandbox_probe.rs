//! Probe binary for the landlock integration test (`tests/landlock.rs`).
//!
//! Usage: `sandbox_probe <write-root> <denied-dir>`
//!
//! Under a landlock confinement with `write-root` as the only write root,
//! the probe must be able to create a file inside `write-root`, must be
//! refused creating one inside `denied-dir` at the OS layer, and must still
//! be able to read a system file (the read floor that keeps the loader
//! working). Every check prints one line; the final line is `RESULT:PASS`
//! or `RESULT:FAIL` and the exit code matches.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (write_root, denied) = match args.as_slice() {
        [_, write_root, denied] => (write_root.clone(), denied.clone()),
        _ => {
            eprintln!("usage: sandbox_probe <write-root> <denied-dir>");
            std::process::exit(2);
        }
    };

    let mut ok = true;
    let check = |label: &str, passed: bool, detail: String, ok: &mut bool| {
        println!("{label}:{}", if passed { "ok" } else { "FAIL" });
        if !passed {
            println!("  detail: {detail}");
            *ok = false;
        }
    };

    // 1. Creating a file inside the write root must succeed.
    let inside = std::path::Path::new(&write_root).join("probe-inside.txt");
    match std::fs::write(&inside, b"x") {
        Ok(()) => check("write-inside", true, String::new(), &mut ok),
        Err(error) => check("write-inside", false, error.to_string(), &mut ok),
    }

    // 2. Creating a file outside every write root must be refused by the
    // kernel (EACCES/EROFS), not by application logic.
    let outside = std::path::Path::new(&denied).join("probe-outside.txt");
    match std::fs::write(&outside, b"x") {
        Ok(()) => check(
            "write-outside",
            false,
            "succeeded (not confined!)".into(),
            &mut ok,
        ),
        Err(error) => check("write-outside", true, error.to_string(), &mut ok),
    }

    // 3. Reading a system file stays allowed (the read floor).
    let read_ok = std::fs::read_to_string("/etc/passwd").is_ok();
    check(
        "read-passwd",
        read_ok,
        "cannot read /etc/passwd".into(),
        &mut ok,
    );

    println!("RESULT:{}", if ok { "PASS" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}
