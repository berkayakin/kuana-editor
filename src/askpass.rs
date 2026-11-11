//! Git's and SSH's questions (a password, a key's passphrase, whether to
//! trust a new host) answered in Kuana's window instead of a terminal.
//!
//! Git commands that may ask (`git::asking`: the ones the user starts) run
//! with `GIT_ASKPASS` and `SSH_ASKPASS` set to Kuana's own executable and
//! `KUANA_ASKPASS` to this process's loopback port and a random token. Git or
//! SSH starts that executable with the question as its argument; it notices
//! `KUANA_ASKPASS` before anything else (`run_if_asked`, from `main`), sends
//! the question here, waits for the answer and prints it, which is what an
//! askpass program does. Here, each question goes to the app as a `Question`
//! (`serve`), which a workspace shows as a dialog and answers.
//!
//! Background work (the periodic fetch) never asks: it stays quiet instead.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use futures::channel::{mpsc, oneshot};
use serde::{Deserialize, Serialize};

/// The variable that makes the executable an askpass program: `<port>:<token>`.
const VARIABLE: &str = "KUANA_ASKPASS";
/// The longest question or answer taken.
const MAX_LINE: u64 = 64 * 1024;

/// What the askpass program sends.
#[derive(Serialize, Deserialize)]
struct Asked {
    token: String,
    prompt: String,
    /// SSH asks to confirm (a new host's key) rather than for text.
    confirm: bool,
}

/// What it gets back: the answer, or none when the user cancelled.
#[derive(Serialize, Deserialize)]
struct Answered {
    answer: Option<String>,
}

/// A question waiting for the user.
pub struct Question {
    /// As Git or SSH put it ("Password for 'https://me@github.com': ").
    pub prompt: String,
    /// A yes/no question (SSH's new host key), answered "yes" or "no".
    pub confirm: bool,
    answer: oneshot::Sender<Option<String>>,
}

impl Question {
    /// Whether what's typed should show as bullets.
    pub fn secret(&self) -> bool {
        let prompt = self.prompt.to_lowercase();
        !self.confirm && !prompt.starts_with("username") && ["password", "passphrase", "token", "pin"].iter().any(|w| prompt.contains(w))
    }

    /// Answers it; `None` cancels (the Git command fails as it would without one).
    pub fn answer(self, answer: Option<String>) {
        let _ = self.answer.send(answer);
    }
}

/// Where the askpass program finds this process.
struct Server {
    port: u16,
    token: String,
}

static SERVER: OnceLock<Server> = OnceLock::new();

/// How many questions went unanswered (cancelled), ever.
static CANCELLED: AtomicU64 = AtomicU64::new(0);

/// How many questions have been cancelled so far: a Git command that failed
/// while this went up failed because the user cancelled, not with an error
/// worth showing (Git says "unable to read askpass response").
pub fn cancellations() -> u64 {
    CANCELLED.load(Ordering::Relaxed)
}

/// Starts taking questions (once per process); they come out of the
/// receiver. `None` when it couldn't listen: Git commands then don't ask.
pub fn serve() -> Option<mpsc::UnboundedReceiver<Question>> {
    if SERVER.get().is_some() {
        return None;
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).ok()?;
    let port = listener.local_addr().ok()?.port();
    let token = crate::instance::random_token();
    let (tx, rx) = mpsc::unbounded();
    let accepted = token.clone();
    std::thread::Builder::new()
        .name("askpass".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let (tx, token) = (tx.clone(), accepted.clone());
                // One thread per question: an unanswered one holds only its own.
                let _ = std::thread::Builder::new().name("askpass question".into()).spawn(move || take_question(stream, &token, &tx));
            }
        })
        .ok()?;
    SERVER.set(Server { port, token }).ok()?;
    Some(rx)
}

/// Reads one question, hands it to the app and writes back the answer.
fn take_question(stream: TcpStream, token: &str, questions: &mpsc::UnboundedSender<Question>) -> Option<()> {
    let mut reader = BufReader::new(stream.try_clone().ok()?).take(MAX_LINE);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let asked: Asked = serde_json::from_str(&line).ok()?;
    if asked.token != token {
        return None;
    }
    let (answer, answered) = oneshot::channel();
    questions.unbounded_send(Question { prompt: asked.prompt, confirm: asked.confirm, answer }).ok()?;
    // A question nobody can answer (no window) is a cancel.
    let answer = futures::executor::block_on(answered).ok().flatten();
    if answer.is_none() {
        CANCELLED.fetch_add(1, Ordering::Relaxed);
    }
    let mut reply = serde_json::to_string(&Answered { answer }).ok()?;
    reply.push('\n');
    (&stream).write_all(reply.as_bytes()).ok()
}

/// The environment that lets a Git command ask through Kuana, while it serves.
pub fn env() -> Option<Vec<(&'static str, String)>> {
    let server = SERVER.get()?;
    let exe = std::env::current_exe().ok()?.to_string_lossy().into_owned();
    Some(vec![
        ("GIT_ASKPASS", exe.clone()),
        ("SSH_ASKPASS", exe),
        // Without a terminal SSH would use it anyway only with a DISPLAY.
        ("SSH_ASKPASS_REQUIRE", "force".into()),
        (VARIABLE, format!("{}:{}", server.port, server.token)),
    ])
}

/// This process was started by Git or SSH to ask a question: asks the Kuana
/// that ran the command, prints the answer and returns the exit code (1 when
/// cancelled or unreachable). `None` for a normal launch.
pub fn run_if_asked() -> Option<i32> {
    let address = std::env::var(VARIABLE).ok()?;
    let (port, token) = address.split_once(':')?;
    let prompt = std::env::args().nth(1).unwrap_or_default();
    let confirm = std::env::var("SSH_ASKPASS_PROMPT").is_ok_and(|kind| kind == "confirm");
    Some(match ask(port, token, prompt, confirm) {
        Some(answer) => {
            let mut out = std::io::stdout();
            let _ = writeln!(out, "{answer}");
            let _ = out.flush();
            0
        }
        None => 1,
    })
}

fn ask(port: &str, token: &str, prompt: String, confirm: bool) -> Option<String> {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port.parse::<u16>().ok()?)).ok()?;
    let mut line = serde_json::to_string(&Asked { token: token.to_string(), prompt, confirm }).ok()?;
    line.push('\n');
    stream.write_all(line.as_bytes()).ok()?;
    // No time limit: the user is typing.
    let mut reply = String::new();
    BufReader::new(stream).take(MAX_LINE).read_line(&mut reply).ok()?;
    serde_json::from_str::<Answered>(&reply).ok()?.answer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_travel_to_the_app_and_answers_back() {
        let mut questions = serve().expect("listens");
        let server = SERVER.get().unwrap();
        let (port, token) = (server.port.to_string(), server.token.clone());
        let asking = std::thread::spawn({
            let (port, token) = (port.clone(), token.clone());
            move || ask(&port, &token, "Password for 'https://me@example.com': ".into(), false)
        });
        let question = futures::executor::block_on(futures::StreamExt::next(&mut questions)).unwrap();
        assert!(question.secret());
        assert_eq!(question.prompt, "Password for 'https://me@example.com': ");
        question.answer(Some("s3cret".into()));
        assert_eq!(asking.join().unwrap().as_deref(), Some("s3cret"));

        // Cancelled, and a wrong token gets nothing.
        let asking = std::thread::spawn({
            let (port, token) = (port.clone(), token.clone());
            move || ask(&port, &token, "Username for 'https://example.com': ".into(), false)
        });
        let question = futures::executor::block_on(futures::StreamExt::next(&mut questions)).unwrap();
        assert!(!question.secret());
        question.answer(None);
        assert_eq!(asking.join().unwrap(), None);
        assert_eq!(ask(&port, "wrong", "Password: ".into(), false), None);
    }
}
