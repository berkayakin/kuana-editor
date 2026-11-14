//! `kuana-server`: Kuana's headless side for remote development, run on the
//! remote machine and spoken to over its standard input and output.

fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("kuana-server {}", env!("CARGO_PKG_VERSION"));
    }
}
