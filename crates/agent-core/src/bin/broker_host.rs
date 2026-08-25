//! 进程外效果协调器宿主：以唯一参数接收预留日志路径，在 stdin/
//! stdout 上按行驱动协调器协议。stdin 关闭（EOF）或收到 shutdown
//! 即干净退出；日志损坏或协议违规以非零码失败。

use std::io::BufReader;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(journal_path) = args.next() else {
        eprintln!("usage: broker_host <journal-path>");
        std::process::exit(2);
    };
    let journal = match agent_core::ReservationJournal::open(std::path::Path::new(&journal_path)) {
        Ok(journal) => journal,
        Err(error) => {
            eprintln!("broker_host: open journal: {error}");
            std::process::exit(1);
        }
    };
    let mut input = BufReader::new(std::io::stdin().lock());
    let mut output = std::io::stdout().lock();
    if let Err(error) = agent_core::serve_broker_lines(&mut input, &mut output, &journal) {
        eprintln!("broker_host: {error}");
        std::process::exit(1);
    }
}
