//! Shell integration, as VS Code's and iTerm's: the shell marks its prompts
//! and commands with OSC 133 (`A` prompt, `B` input, `C` running, `D;code`
//! done) and says its folder with OSC 7. Kuana's own scripts make zsh, bash
//! and fish do it (`integration`); shells that do it by themselves count too.
//!
//! alacritty_terminal ignores those sequences, and its event loop parses the
//! shell's output with the terminal locked, so where a mark lands can't be
//! asked then. `MarkFilter`, between the pty and the parser (`run_log::Teed`),
//! turns a prompt (`A` … `B`) into an OSC 8 hyperlink with a `kuana-prompt:`
//! URI around the prompt's text: the grid then keeps where each prompt is,
//! through scrolling and resizing, and the view finds them in its cells. What
//! has no place (a command's start and exit code, the folder) goes to the
//! view as `Mark`s.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// The URI scheme of a prompt's hyperlink; its path is the prompt's number.
pub const PROMPT_URI: &str = "kuana-prompt:";
/// Longest escape sequence held back: an OSC longer than this isn't ours,
/// and goes through as it is.
const MAX_OSC: usize = 4096;

/// What the shell said that has no place in the grid.
#[derive(Clone, Debug, PartialEq)]
pub enum Mark {
    /// Prompt `n` started (its text is a `kuana-prompt:n` hyperlink).
    Prompt(u32),
    /// The command typed at prompt `n` started running.
    Started(u32),
    /// The command typed at prompt `n` ended, with its exit code if said.
    Finished(u32, Option<i32>),
    /// The shell's folder now.
    Cwd(PathBuf),
}

/// The marks found since the view last took them (`take`).
#[derive(Clone, Default)]
pub struct Marks(Arc<Mutex<Vec<Mark>>>);

impl Marks {
    pub fn take(&self) -> Vec<Mark> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn push(&self, marks: Vec<Mark>) {
        if !marks.is_empty() {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).extend(marks);
        }
    }
}

/// Rewrites the shell's marks on their way to the parser (see the module docs).
#[derive(Default)]
pub struct MarkFilter {
    /// An escape sequence cut by the end of a read, until the rest comes.
    partial: Vec<u8>,
    /// The last prompt's number (0: none yet).
    prompt: u32,
    /// A prompt's hyperlink is open (between `A` and `B`).
    open: bool,
    marks: Marks,
}

impl MarkFilter {
    pub fn new(marks: Marks) -> Self {
        MarkFilter { marks, ..Default::default() }
    }

    /// `input` with the marks rewritten, appended to `out`.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        let mut found = Vec::new();
        let mut bytes = std::mem::take(&mut self.partial);
        bytes.extend_from_slice(input);
        let mut i = 0;
        while i < bytes.len() {
            let start = i;
            if bytes[i] != 0x1b {
                // Up to the next escape, as it is.
                let next = bytes[i..].iter().position(|&b| b == 0x1b).map_or(bytes.len(), |p| i + p);
                out.extend_from_slice(&bytes[i..next]);
                i = next;
                continue;
            }
            match bytes.get(i + 1) {
                None => {
                    self.partial = bytes[i..].to_vec();
                    break;
                }
                Some(b']') => {}
                Some(_) => {
                    out.push(0x1b);
                    i += 1;
                    continue;
                }
            }
            // An OSC: up to BEL or ESC \.
            let body_start = i + 2;
            let mut end = None;
            let mut j = body_start;
            while j < bytes.len() {
                match bytes[j] {
                    0x07 => {
                        end = Some((j, j + 1));
                        break;
                    }
                    0x1b if bytes.get(j + 1) == Some(&b'\\') => {
                        end = Some((j, j + 2));
                        break;
                    }
                    0x1b if j + 1 == bytes.len() => break,
                    _ => j += 1,
                }
            }
            let Some((body_end, next)) = end else {
                if bytes.len() - start > MAX_OSC {
                    // Not one of ours (nor finished): let it through.
                    out.extend_from_slice(&bytes[start..]);
                } else {
                    self.partial = bytes[start..].to_vec();
                }
                break;
            };
            let body = &bytes[body_start..body_end];
            if !self.mark(body, out, &mut found) {
                out.extend_from_slice(&bytes[start..next]);
            }
            i = next;
        }
        self.marks.push(found);
    }

    /// An OSC's body: whether it was a mark (handled, not passed on).
    fn mark(&mut self, body: &[u8], out: &mut Vec<u8>, found: &mut Vec<Mark>) -> bool {
        if let Some(rest) = body.strip_prefix(b"133;") {
            let mut parts = rest.split(|&b| b == b';');
            match parts.next() {
                Some(b"A") => {
                    self.close(out);
                    self.prompt += 1;
                    out.extend_from_slice(format!("\x1b]8;;{PROMPT_URI}{}\x1b\\", self.prompt).as_bytes());
                    self.open = true;
                    found.push(Mark::Prompt(self.prompt));
                }
                Some(b"B") => self.close(out),
                Some(b"C") => {
                    self.close(out);
                    if self.prompt > 0 {
                        found.push(Mark::Started(self.prompt));
                    }
                }
                Some(b"D") => {
                    self.close(out);
                    let code = parts.next().and_then(|c| std::str::from_utf8(c).ok()?.trim().parse().ok());
                    if self.prompt > 0 {
                        found.push(Mark::Finished(self.prompt, code));
                    }
                }
                // Other FinalTerm marks: nothing to keep.
                _ => {}
            }
            return true;
        }
        if let Some(rest) = body.strip_prefix(b"7;") {
            if let Some(path) = std::str::from_utf8(rest).ok().and_then(file_url_path) {
                found.push(Mark::Cwd(path));
            }
            return true;
        }
        false
    }

    /// Ends the prompt's hyperlink if it's open.
    fn close(&mut self, out: &mut Vec<u8>) {
        if self.open {
            out.extend_from_slice(b"\x1b]8;;\x1b\\");
            self.open = false;
        }
    }
}

/// `file://host/path` (percent-encoded) as a path.
fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let mut bytes = Vec::with_capacity(path.len());
    let raw = path.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%'
            && let Some(byte) = raw.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok())
        {
            bytes.push(byte);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(bytes).ok()?;
    // Windows: `/C:/Users/…`.
    let path = if cfg!(windows) && path.len() > 2 && path.as_bytes()[2] == b':' { path[1..].to_string() } else { path };
    Some(PathBuf::from(path))
}

/// The prompt number in a hyperlink's URI, if it's a prompt's.
pub fn prompt_of(uri: &str) -> Option<u32> {
    uri.strip_prefix(PROMPT_URI)?.parse().ok()
}

/// How to start `program` with Kuana's integration: the command line to
/// use instead (None: as it is) and the environment to add. None when the
/// shell isn't one Kuana has a script for. `login`: start it as a login
/// shell (as Kuana's terminals do).
pub fn integration(program: &str, args: &[String], dir: &Path) -> Option<(Option<Vec<String>>, Vec<(String, String)>)> {
    let name = Path::new(program).file_name()?.to_str()?.trim_start_matches('-');
    match name {
        "zsh" => {
            let zdotdir = write_scripts(dir, "zsh", ZSH)?;
            let mut env = vec![("ZDOTDIR".to_string(), zdotdir.display().to_string())];
            if let Ok(user) = std::env::var("ZDOTDIR") {
                env.push(("KUANA_USER_ZDOTDIR".to_string(), user));
            }
            Some((None, env))
        }
        "bash" => {
            let folder = write_scripts(dir, "bash", BASH)?;
            // A login shell skips `--init-file`: the script reads the profile itself.
            let login = args.is_empty() || args.iter().any(|a| a == "-l" || a == "--login");
            let mut command = vec![program.to_string(), "--init-file".to_string(), folder.join("kuana.bash").display().to_string()];
            command.extend(args.iter().filter(|a| *a != "-l" && *a != "--login").cloned());
            let env = vec![("KUANA_BASH_LOGIN".to_string(), if login { "1" } else { "" }.to_string())];
            Some((Some(command), env))
        }
        "fish" => {
            let folder = write_scripts(dir, "fish", FISH)?;
            let mut command = vec![program.to_string()];
            command.extend(args.iter().cloned());
            if args.is_empty() {
                command.push("-l".to_string());
            }
            command.extend(["--init-command".to_string(), format!("source '{}'", folder.join("kuana.fish").display())]);
            Some((Some(command), Vec::new()))
        }
        _ => None,
    }
}

/// Writes a shell's scripts into `dir/<shell>/` (only when they changed),
/// returning that folder.
fn write_scripts(dir: &Path, shell: &str, files: &[(&str, &str)]) -> Option<PathBuf> {
    let folder = dir.join(shell);
    // On a remote host for its terminals (`crate::fs`).
    crate::fs::create_dir_all(&folder).ok()?;
    for (name, text) in files {
        let path = folder.join(name);
        if crate::fs::read_to_string(&path).ok().as_deref() != Some(*text) {
            crate::fs::write(&path, text).ok()?;
        }
    }
    Some(folder)
}

/// zsh reads its files from `$ZDOTDIR`: each of Kuana's reads the user's own
/// (from where they keep them) with `ZDOTDIR` theirs meanwhile, and
/// `.zshrc` adds the hooks and leaves `ZDOTDIR` as the user's.
const ZSH: &[(&str, &str)] = &[
    (".zshenv", ZSH_STEP_ENV),
    (".zprofile", "__kuana_source .zprofile\n"),
    // No `.zlogin`: zsh reads it after `.zshrc`, from the user's `ZDOTDIR` by then.
    (".zshrc", ZSH_RC),
];

const ZSH_STEP_ENV: &str = r#"# Kuana's shell integration: reads the user's own files, then marks prompts.
KUANA_ZDOTDIR="$ZDOTDIR"
__kuana_source() {
  ZDOTDIR="${KUANA_USER_ZDOTDIR:-$HOME}"
  [[ -f "$ZDOTDIR/$1" ]] && builtin source "$ZDOTDIR/$1"
  KUANA_USER_ZDOTDIR="$ZDOTDIR"
  ZDOTDIR="$KUANA_ZDOTDIR"
}
__kuana_source .zshenv
"#;

const ZSH_RC: &str = r#"__kuana_source .zshrc
ZDOTDIR="$KUANA_USER_ZDOTDIR"
unset KUANA_USER_ZDOTDIR KUANA_ZDOTDIR
unfunction __kuana_source 2>/dev/null
if [[ -z "$__kuana_hooked" ]]; then
  __kuana_hooked=1
  __kuana_ran=
  __kuana_precmd() {
    local code=$?
    [[ -n "$__kuana_ran" ]] && builtin printf '\e]133;D;%s\a' "$code"
    __kuana_ran=
    builtin printf '\e]7;file://%s%s\a' "${HOST}" "${PWD// /%20}"
    # Themes may build the prompt anew each time: marked after them.
    if [[ "$PS1" != *'133;A'* ]]; then
      PS1=$'%{\e]133;A\a%}'"$PS1"$'%{\e]133;B\a%}'
    fi
  }
  __kuana_preexec() {
    __kuana_ran=1
    builtin printf '\e]133;C\a'
  }
  autoload -Uz add-zsh-hook
  add-zsh-hook precmd __kuana_precmd
  add-zsh-hook preexec __kuana_preexec
fi
"#;

const BASH: &[(&str, &str)] = &[("kuana.bash", BASH_RC)];

const BASH_RC: &str = r#"# Kuana's shell integration: reads the user's own files, then marks prompts.
if [ -n "$KUANA_BASH_LOGIN" ]; then
  [ -r /etc/profile ] && . /etc/profile
  for __kuana_f in ~/.bash_profile ~/.bash_login ~/.profile; do
    if [ -r "$__kuana_f" ]; then . "$__kuana_f"; break; fi
  done
  unset __kuana_f
else
  [ -r ~/.bashrc ] && . ~/.bashrc
fi
unset KUANA_BASH_LOGIN
if [ -z "$__kuana_hooked" ]; then
  __kuana_hooked=1
  __kuana_ran=0
  # Until the first prompt is drawn, what runs (this script) isn't a command.
  __kuana_in_prompt=1
  __kuana_preexec() {
    [ "$__kuana_in_prompt" = 1 ] && return
    [ "$__kuana_ran" = 1 ] && return
    [ -n "$COMP_LINE" ] && return
    __kuana_ran=1
    builtin printf '\e]133;C\a'
  }
  __kuana_precmd() {
    local code=$?
    __kuana_in_prompt=1
    [ "$__kuana_ran" = 1 ] && builtin printf '\e]133;D;%s\a' "$code"
    __kuana_ran=0
    builtin printf '\e]7;file://%s%s\a' "$HOSTNAME" "${PWD// /%20}"
    case "$PS1" in
      *'133;A'*) ;;
      *) PS1='\[\e]133;A\a\]'"$PS1"'\[\e]133;B\a\]' ;;
    esac
  }
  __kuana_prompt_done() { __kuana_in_prompt=0; }
  trap '__kuana_preexec' DEBUG
  PROMPT_COMMAND="__kuana_precmd${PROMPT_COMMAND:+; $PROMPT_COMMAND}; __kuana_prompt_done"
fi
"#;

const FISH: &[(&str, &str)] = &[("kuana.fish", FISH_RC)];

const FISH_RC: &str = r#"# Kuana's shell integration: marks prompts (fish has read the user's config).
if not set -q __kuana_hooked
  set -g __kuana_hooked 1
  set -g __kuana_ran 0
  function __kuana_preexec --on-event fish_preexec
    set -g __kuana_ran 1
    printf '\e]133;C\a'
  end
  function __kuana_postexec --on-event fish_postexec
    set -l code $status
    if test "$__kuana_ran" = 1
      printf '\e]133;D;%s\a' $code
    end
    set -g __kuana_ran 0
  end
  function __kuana_cwd --on-event fish_prompt
    printf '\e]7;file://%s%s\a' (hostname) (string replace -a ' ' '%20' -- $PWD)
  end
  if functions -q fish_prompt
    functions -c fish_prompt __kuana_user_prompt
    function fish_prompt
      printf '\e]133;A\a'
      __kuana_user_prompt
      printf '\e]133;B\a'
    end
  end
end
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn filtered(chunks: &[&[u8]]) -> (String, Vec<Mark>) {
        let marks = Marks::default();
        let mut filter = MarkFilter::new(marks.clone());
        let mut out = Vec::new();
        for chunk in chunks {
            filter.feed(chunk, &mut out);
        }
        (String::from_utf8_lossy(&out).into_owned(), marks.take())
    }

    #[test]
    fn prompts_become_hyperlinks_and_commands_marks() {
        let (out, marks) = filtered(&[b"\x1b]133;A\x07~ $ \x1b]133;B\x07ls\r\n\x1b]133;C\x07a b\r\n\x1b]133;D;1\x07\x1b]133;A\x07~ $ \x1b]133;B\x07"]);
        assert_eq!(out, "\x1b]8;;kuana-prompt:1\x1b\\~ $ \x1b]8;;\x1b\\ls\r\na b\r\n\x1b]8;;kuana-prompt:2\x1b\\~ $ \x1b]8;;\x1b\\");
        assert_eq!(marks, [Mark::Prompt(1), Mark::Started(1), Mark::Finished(1, Some(1)), Mark::Prompt(2)]);
    }

    #[test]
    fn sequences_split_across_reads_wait_for_their_end() {
        let (out, marks) = filtered(&[b"x\x1b", b"]133;", b"A\x1b", b"\\> \x1b]13", b"3;B\x07y"]);
        assert_eq!(out, "x\x1b]8;;kuana-prompt:1\x1b\\> \x1b]8;;\x1b\\y");
        assert_eq!(marks, [Mark::Prompt(1)]);
    }

    #[test]
    fn other_sequences_pass_through() {
        let title = b"\x1b]0;my title\x07\x1b[31mred\x1b[0m\x1b]8;;https://a.b\x1b\\link\x1b]8;;\x1b\\";
        let (out, marks) = filtered(&[title]);
        assert_eq!(out.as_bytes(), title);
        assert!(marks.is_empty());
        // An unfinished OSC longer than any mark goes through as it is.
        let long = [b"\x1b]999;".as_slice(), &vec![b'x'; MAX_OSC + 10]].concat();
        let (out, _) = filtered(&[&long]);
        assert_eq!(out.len(), long.len());
    }

    #[test]
    fn the_folder_comes_from_osc_7() {
        let (out, marks) = filtered(&[b"\x1b]7;file://host/Users/me/my%20dir\x07$ "]);
        assert_eq!(out, "$ ");
        assert_eq!(marks, [Mark::Cwd(PathBuf::from("/Users/me/my dir"))]);
        assert_eq!(prompt_of("kuana-prompt:12"), Some(12));
        assert_eq!(prompt_of("https://x"), None);
    }

    /// The scripts in real shells (when installed), fed commands through a
    /// pipe with an empty home, so the user's own files don't run.
    #[test]
    fn zsh_and_bash_mark_prompts_commands_and_folders() {
        let dir = std::env::temp_dir().join(format!("kuana-shell-run-{}", std::process::id()));
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        for shell in ["/bin/zsh", "/bin/bash"] {
            if !Path::new(shell).exists() {
                continue;
            }
            let (command, env) = integration(shell, &[], &dir).unwrap();
            let mut command = command.unwrap_or_else(|| vec![shell.to_string()]);
            command.push("-i".to_string());
            // One pipe for both, as a terminal has: prompts (stderr) and marks (stdout) in order.
            let (mut reader, writer) = std::io::pipe().unwrap();
            let mut child = std::process::Command::new(&command[0])
                .args(&command[1..])
                .env_clear()
                .env("HOME", &home)
                .env("PATH", "/usr/bin:/bin")
                .envs(env)
                .env("KUANA_USER_ZDOTDIR", &home)
                .current_dir(&home)
                .stdin(std::process::Stdio::piped())
                .stdout(writer.try_clone().unwrap())
                .stderr(writer)
                .spawn()
                .unwrap();
            use std::io::{Read, Write};
            child.stdin.take().unwrap().write_all(b"echo hi\nfalse\nexit\n").unwrap();
            let mut output = Vec::new();
            reader.read_to_end(&mut output).unwrap();
            child.wait().unwrap();
            let marks = Marks::default();
            let mut out = Vec::new();
            MarkFilter::new(marks.clone()).feed(&output, &mut out);
            let marks = marks.take();
            let finished: Vec<Option<i32>> = marks.iter().filter_map(|m| if let Mark::Finished(_, code) = m { Some(*code) } else { None }).collect();
            assert_eq!(finished, [Some(0), Some(1)], "{shell}: {marks:?}");
            assert!(marks.iter().any(|m| matches!(m, Mark::Prompt(_))), "{shell}");
            assert!(marks.iter().any(|m| matches!(m, Mark::Cwd(p) if p.ends_with("home"))), "{shell}: {marks:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn integration_knows_zsh_bash_and_fish() {
        let dir = std::env::temp_dir().join(format!("kuana-shell-{}", std::process::id()));
        let (command, env) = integration("/bin/zsh", &[], &dir).unwrap();
        assert!(command.is_none());
        assert_eq!(env[0], ("ZDOTDIR".to_string(), dir.join("zsh").display().to_string()));
        assert!(dir.join("zsh/.zshrc").is_file());
        let (command, env) = integration("/bin/bash", &["-l".into()], &dir).unwrap();
        assert_eq!(command.unwrap()[1..3], ["--init-file".to_string(), dir.join("bash/kuana.bash").display().to_string()]);
        assert_eq!(env, [("KUANA_BASH_LOGIN".to_string(), "1".to_string())]);
        let (command, _) = integration("/opt/homebrew/bin/fish", &[], &dir).unwrap();
        assert_eq!(command.unwrap()[1], "-l");
        assert!(integration("/bin/sh", &[], &dir).is_none());
        assert!(integration("nu", &[], &dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
