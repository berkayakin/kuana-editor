//! `kuana-server`: Kuana's headless side for remote development
//! (`plans/remote-dev.md`). Kuana starts it on the remote machine (through
//! `ssh`) and talks to it over its standard input and output; see
//! `kuana_core::remote::server`. Nothing else may be printed on stdout.

fn main() {
    // Started by Git or SSH to ask a question (see `server::askpass`).
    #[cfg(unix)]
    if let Some(code) = kuana_core::remote::server::askpass::ask() {
        std::process::exit(code);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") {
        println!("kuana-server {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|a| a == "--stdio") {
        let stdin = std::io::stdin();
        kuana_core::remote::server::serve(stdin.lock(), std::io::stdout(), Default::default());
        return;
    }
    eprintln!("usage: kuana-server --stdio | --version");
    std::process::exit(2);
}
