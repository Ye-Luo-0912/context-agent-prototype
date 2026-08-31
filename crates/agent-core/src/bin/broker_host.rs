//! 进程外效果协调器宿主：以唯一参数接收预留日志路径，在 stdin/
//! stdout 上按换行终止帧驱动协调器协议。零字节 EOF 或收到 shutdown
//! 即干净退出；日志损坏、EOF 前残帧、空帧、非 UTF-8、malformed
//! JSON、超限帧都以非零码失败（fail closed）。

#[tokio::main]
async fn main() {
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
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    if let Err(error) = agent_core::serve_broker_frames(&mut input, &mut output, &journal).await {
        eprintln!("broker_host: {error}");
        std::process::exit(1);
    }
}
