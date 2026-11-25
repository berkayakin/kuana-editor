//! What Kuana asks of the operating system that the crates it uses don't
//! offer: keeping the computer awake while an agent works (`power`) and
//! telling the user something finished or needs them (`notify`). GPUI-free,
//! like `kuana-core`, and apart from it so the remote server doesn't carry
//! these frameworks; on its own it can also be checked for the other systems
//! from one machine (`cargo check -p kuana-os --target x86_64-pc-windows-msvc`).

pub mod notify;
pub mod power;
