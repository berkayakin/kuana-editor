//! A terminal whose shell runs on a remote host (`kuana-server`'s `pty/*`).
//! alacritty's event loop polls a pty's file descriptor; here that's one end
//! of a local socket pair (a Unix one, on Windows a loopback TCP one, which
//! IOCP polls). A thread writes the host's output into the other end
//! (`ours`), another reads what the event loop writes there (keys) and sends
//! it to the host. The shell's end comes on a second pair, alacritty's
//! child-event token, as a byte once all its output is in.

use std::{
    io::{Read, Write},
    net::Shutdown,
    sync::{Arc, Mutex, mpsc},
};

use alacritty_terminal::{
    event::{OnResize, WindowSize},
    tty::{ChildEvent, EventedPty, EventedReadWrite},
};
use kuana_core::remote::{RemoteHost, client::PtySinks};
use polling::{Event, PollMode, Poller};

/// alacritty's tokens (`tty::unix`'s are crate-private).
#[cfg(unix)]
const READ_WRITE_TOKEN: usize = 0;
#[cfg(windows)]
const READ_WRITE_TOKEN: usize = alacritty_terminal::tty::windows::PTY_READ_WRITE_TOKEN;
#[cfg(unix)]
const CHILD_EVENT_TOKEN: usize = 1;
#[cfg(windows)]
const CHILD_EVENT_TOKEN: usize = alacritty_terminal::tty::windows::PTY_CHILD_EVENT_TOKEN;

#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type Stream = std::net::TcpStream;

/// Two connected streams.
#[cfg(unix)]
fn pair() -> std::io::Result<(Stream, Stream)> {
    Stream::pair()
}

/// Two connected loopback streams: the accepted end must be the one just
/// connected, not another local program that was quicker.
#[cfg(windows)]
fn pair() -> std::io::Result<(Stream, Stream)> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let ours = Stream::connect(listener.local_addr()?)?;
    loop {
        let (theirs, from) = listener.accept()?;
        if from == ours.local_addr()? {
            ours.set_nodelay(true)?;
            theirs.set_nodelay(true)?;
            return Ok((theirs, ours));
        }
    }
}

/// What the poller takes to forget a stream.
#[cfg(unix)]
fn source(stream: &Stream) -> std::os::fd::BorrowedFd<'_> {
    std::os::fd::AsFd::as_fd(stream)
}
#[cfg(windows)]
fn source(stream: &Stream) -> std::os::windows::io::BorrowedSocket<'_> {
    std::os::windows::io::AsSocket::as_socket(stream)
}

/// A wait status for an exit code.
#[cfg(unix)]
fn exit_status(code: i32) -> std::process::ExitStatus {
    // The code in the high byte (a signal: none).
    std::os::unix::process::ExitStatusExt::from_raw((code & 0xff) << 8)
}
#[cfg(windows)]
fn exit_status(code: i32) -> std::process::ExitStatus {
    std::os::windows::process::ExitStatusExt::from_raw(code as u32)
}

pub struct RemotePty {
    host: Arc<RemoteHost>,
    id: u64,
    /// The event loop's end.
    stream: Stream,
    /// Readable once the shell ended.
    ended: Stream,
    code: Arc<Mutex<Option<Option<i32>>>>,
}

enum Out {
    Data(Vec<u8>),
    End(Option<i32>),
}

impl RemotePty {
    /// Starts `shell` (host paths) in `cwd` there.
    pub fn start(host: Arc<RemoteHost>, shell: &[String], cwd: Option<&str>, env: &[(String, String)]) -> std::io::Result<RemotePty> {
        let (stream, ours) = pair()?;
        let (ended, ended_ours) = pair()?;
        stream.set_nonblocking(true)?;
        ended.set_nonblocking(true)?;
        let code = Arc::new(Mutex::new(None));
        // In order: the shell's output, then its end.
        let (tx, rx) = mpsc::channel::<Out>();
        let (data_tx, end_tx) = (Mutex::new(tx.clone()), Mutex::new(tx));
        let sinks = PtySinks {
            output: Box::new(move |data| {
                let _ = data_tx.lock().unwrap_or_else(|e| e.into_inner()).send(Out::Data(data));
            }),
            exit: Box::new(move |code| {
                let _ = end_tx.lock().unwrap_or_else(|e| e.into_inner()).send(Out::End(code));
            }),
        };
        let id = host.pty_open(shell, cwd, env, (80, 24), sinks)?;
        let mut writer = ours.try_clone()?;
        let recorded = code.clone();
        std::thread::Builder::new().name("remote-pty-out".into()).spawn(move || {
            let mut ended_ours = ended_ours;
            for message in rx {
                match message {
                    Out::Data(data) => {
                        if writer.write_all(&data).is_err() {
                            break;
                        }
                    }
                    Out::End(code) => {
                        *recorded.lock().unwrap_or_else(|e| e.into_inner()) = Some(code);
                        let _ = writer.shutdown(Shutdown::Write);
                        let _ = ended_ours.write_all(&[1]);
                        break;
                    }
                }
            }
        })?;
        let (keys_host, mut keys) = (host.clone(), ours);
        std::thread::Builder::new().name("remote-pty-in".into()).spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n) = keys.read(&mut buf) {
                if n == 0 {
                    break;
                }
                keys_host.pty_input(id, &buf[..n]);
            }
        })?;
        Ok(RemotePty { host, id, stream, ended, code })
    }

    pub fn host(&self) -> (Arc<RemoteHost>, u64) {
        (self.host.clone(), self.id)
    }
}

impl Drop for RemotePty {
    fn drop(&mut self) {
        self.host.pty_close(self.id);
    }
}

impl EventedReadWrite for RemotePty {
    type Reader = Stream;
    type Writer = Stream;

    unsafe fn register(&mut self, poller: &Arc<Poller>, mut interest: Event, mode: PollMode) -> std::io::Result<()> {
        interest.key = READ_WRITE_TOKEN;
        // SAFETY: the streams outlive their registration (deregistered before they're dropped).
        unsafe {
            poller.add_with_mode(&self.stream, interest, mode)?;
            poller.add_with_mode(&self.ended, Event::readable(CHILD_EVENT_TOKEN), PollMode::Level)
        }
    }

    fn reregister(&mut self, poller: &Arc<Poller>, mut interest: Event, mode: PollMode) -> std::io::Result<()> {
        interest.key = READ_WRITE_TOKEN;
        poller.modify_with_mode(&self.stream, interest, mode)?;
        poller.modify_with_mode(&self.ended, Event::readable(CHILD_EVENT_TOKEN), PollMode::Level)
    }

    fn deregister(&mut self, poller: &Arc<Poller>) -> std::io::Result<()> {
        poller.delete(source(&self.stream))?;
        poller.delete(source(&self.ended))
    }

    fn reader(&mut self) -> &mut Stream {
        &mut self.stream
    }

    fn writer(&mut self) -> &mut Stream {
        &mut self.stream
    }
}

impl EventedPty for RemotePty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        let mut byte = [0u8; 1];
        match self.ended.read(&mut byte) {
            Ok(1) => {
                let code = self.code.lock().unwrap_or_else(|e| e.into_inner()).take().flatten();
                Some(ChildEvent::Exited(code.map(exit_status)))
            }
            _ => None,
        }
    }
}

impl OnResize for RemotePty {
    fn on_resize(&mut self, size: WindowSize) {
        self.host.pty_resize(self.id, size.num_cols, size.num_lines);
    }
}
