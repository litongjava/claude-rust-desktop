//! Request-driven terminal input: one OS reader, idle until a consumer asks.
//! History is session-local and restricted to explicit REPL reads. Answers to
//! permission and provider questions never enter completion or history.
use rustyline::{error::ReadlineError, history::DefaultHistory, Editor};
use std::io::{self, BufRead, IsTerminal, Write};
use tokio::sync::{mpsc, oneshot};

#[path = "editor.rs"]
mod editor;

enum Message {
    Commands(Vec<(String, String)>),
    Details(Vec<(String, String)>),
    Read(Request),
}

struct Request {
    prompt: String,
    repl: bool,
    response: oneshot::Sender<io::Result<Option<String>>>,
}

pub struct TerminalInput {
    requests: std::sync::mpsc::Sender<Message>,
    pending: Option<oneshot::Receiver<io::Result<Option<String>>>>,
    fixture: Option<mpsc::UnboundedReceiver<String>>,
}

impl TerminalInput {
    pub fn new() -> Self {
        let (requests, rx) = std::sync::mpsc::channel::<Message>();
        std::thread::spawn(move || {
            let terminal = io::stdin().is_terminal()
                && io::stdout().is_terminal()
                && io::stderr().is_terminal()
                && std::env::var("TERM").as_deref() != Ok("dumb");
            let mut editor = editor::PromptEditor::default();
            let mut plain = io::BufReader::new(io::stdin());
            for message in rx {
                let request = match message {
                    Message::Commands(commands) => {
                        editor.set_commands(commands);
                        continue;
                    }
                    Message::Details(details) => {
                        editor.set_details(details);
                        continue;
                    }
                    Message::Read(request) => request,
                };
                if request.response.is_closed() {
                    continue;
                }
                let result = if terminal {
                    if request.repl {
                        editor.readline(&request.prompt)
                    } else {
                        // Answers have no access to prompt history or completion.
                        match Editor::<(), DefaultHistory>::new() {
                            Ok(mut answer) => map_readline(answer.readline(&request.prompt)),
                            Err(error) => Err(io::Error::other(error.to_string())),
                        }
                    }
                } else {
                    read_plain(&mut plain, &request.prompt)
                };
                let _ = request.response.send(result);
            }
        });
        Self {
            requests,
            pending: None,
            fixture: None,
        }
    }

    pub fn set_commands(&self, commands: Vec<(String, String)>) {
        let _ = self.requests.send(Message::Commands(commands));
    }

    /// Set the last turn's tool titles and full display bodies before reading.
    pub fn set_details(&self, details: Vec<(String, String)>) {
        let _ = self.requests.send(Message::Details(details));
    }

    pub async fn read_prompt(&mut self, prompt: &str) -> io::Result<Option<String>> {
        self.read(prompt, true).await
    }

    pub async fn read_answer(&mut self, prompt: &str) -> io::Result<Option<String>> {
        self.read(prompt, false).await
    }

    // Compatibility for voice consumers; errors/interruption terminate input.
    pub async fn recv(&mut self) -> Option<String> {
        self.read_answer("").await.ok().flatten()
    }

    async fn read(&mut self, prompt: &str, repl: bool) -> io::Result<Option<String>> {
        if let Some(fixture) = self.fixture.as_mut() {
            return Ok(fixture.recv().await);
        }
        if self.pending.is_none() {
            let (response, receive) = oneshot::channel();
            self.requests
                .send(Message::Read(Request {
                    prompt: prompt.into(),
                    repl,
                    response,
                }))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Terminal input closed"))?;
            self.pending = Some(receive);
        }
        // Keep the outstanding response in the handle across select! cancellation.
        // There is never a second reader racing for stdin or losing its next line.
        let result = self.pending.as_mut().unwrap().await;
        self.pending = None;
        result.unwrap_or_else(|_| {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Terminal reader stopped",
            ))
        })
    }
}

#[cfg(test)]
impl From<mpsc::UnboundedReceiver<String>> for TerminalInput {
    fn from(fixture: mpsc::UnboundedReceiver<String>) -> Self {
        let (requests, _) = std::sync::mpsc::channel();
        Self {
            requests,
            pending: None,
            fixture: Some(fixture),
        }
    }
}

fn map_readline(result: rustyline::Result<String>) -> io::Result<Option<String>> {
    match result {
        Ok(line) => Ok(Some(line)),
        Err(ReadlineError::Eof) => Ok(None),
        Err(ReadlineError::Interrupted) => Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Input cancelled",
        )),
        Err(error) => Err(io::Error::other(error.to_string())),
    }
}

fn read_plain(reader: &mut impl BufRead, prompt: &str) -> io::Result<Option<String>> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim_end_matches(['\r', '\n']).into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn piped_input_preserves_unicode_spaces_and_eof() {
        let mut input = io::Cursor::new("  中文  \r\n\nlast");
        assert_eq!(
            read_plain(&mut input, "").unwrap().as_deref(),
            Some("  中文  ")
        );
        assert_eq!(read_plain(&mut input, "").unwrap().as_deref(), Some(""));
        assert_eq!(read_plain(&mut input, "").unwrap().as_deref(), Some("last"));
        assert_eq!(read_plain(&mut input, "").unwrap(), None);
    }
    #[test]
    fn interrupt_is_not_eof_or_a_literal_command() {
        assert_eq!(
            map_readline(Err(ReadlineError::Interrupted))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(map_readline(Err(ReadlineError::Eof)).unwrap().is_none());
    }
}
