//! A terminal whose shell runs on a remote host (`kuana-server`'s `pty/*`).
//! alacritty's event loop polls a pty's file descriptor; here that's one end
//! of a local socket pair. A thread writes the host's output into the other
//! end (`ours`), another reads what the event loop writes there (keys) and
//! sends it to the host. The shell's end comes on a second pair, alacritty's
//! child-event token, as a byte once all its output is in.

use std::{
    io::{Read, Write},
    net::Shutdown,
    os::{
        fd::AsFd,
        unix::{net::UnixStream, process::ExitStatusExt},
    },
    sync::{Arc, Mutex, mpsc},
};

use alacritty_terminal::{
    event::{OnResize, WindowSize},
    tty::{ChildEvent, EventedPty, EventedReadWrite},
};
use kuana_core::remote::{RemoteHost, client::PtySinks};
use polling::{Event, PollMode, Poller};

/// alacritty's tokens on Unix (`tty::unix`, crate-private there).
const READ_WRITE_TOKEN: usize = 0;
const CHILD_EVENT_TOKEN: usize = 1;

pub struct RemotePty {
    host: Arc<RemoteHost>,
    id: u64,
    /// The event loop's end.
    stream: UnixStream,
    /// Readable once the shell ended.
    ended: UnixStream,
    code: Arc<Mutex<Option<Option<i32>>>>,
}

enum Out {
    Data(Vec<u8>),
    End(Option<i32>),
}

impl RemotePty {
    /// Starts `shell` (host paths) in `cwd` there.
    pub fn start(host: Arc<RemoteHost>, shell: &[String], cwd: Option<&str>, env: &[(String, String)]) -> std::io::Result<RemotePty> {
        let (stream, ours) = UnixStream::pair()?;
        let (ended, ended_ours) = UnixStream::pair()?;
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
    type Reader = UnixStream;
    type Writer = UnixStream;

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
        poller.delete(self.stream.as_fd())?;
        poller.delete(self.ended.as_fd())
    }

    fn reader(&mut self) -> &mut UnixStream {
        &mut self.stream
    }

    fn writer(&mut self) -> &mut UnixStream {
        &mut self.stream
    }
}

impl EventedPty for RemotePty {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        let mut byte = [0u8; 1];
        match self.ended.read(&mut byte) {
            Ok(1) => {
                let code = self.code.lock().unwrap_or_else(|e| e.into_inner()).take().flatten();
                // A wait status: the code in the high byte (a signal: none).
                Some(ChildEvent::Exited(code.map(|c| std::process::ExitStatus::from_raw((c & 0xff) << 8))))
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
