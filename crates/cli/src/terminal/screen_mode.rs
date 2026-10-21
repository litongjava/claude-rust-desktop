//! Own the alternate screen for the entire REPL, restoring it on every exit.
use crossterm::{
    cursor::MoveTo,
    execute,
    terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, IsTerminal};

#[derive(Default)]
pub struct ScreenMode {
    fullscreen: bool,
}
impl ScreenMode {
    /// Returns true when the new screen needs its transcript replayed.
    pub fn set(&mut self, mode: &str) -> io::Result<bool> {
        let fullscreen = mode == "fullscreen" && !crate::terminal::screen_reader();
        if fullscreen == self.fullscreen {
            return Ok(false);
        }
        if !io::stderr().is_terminal() {
            return Err(io::Error::other(
                "Fullscreen requires an interactive terminal",
            ));
        }
        if fullscreen {
            execute!(
                io::stderr(),
                EnterAlternateScreen,
                Clear(ClearType::All),
                MoveTo(0, 0)
            )?;
        } else {
            execute!(io::stderr(), LeaveAlternateScreen)?;
        }
        self.fullscreen = fullscreen;
        Ok(fullscreen)
    }
}
impl Drop for ScreenMode {
    fn drop(&mut self) {
        if self.fullscreen {
            let _ = execute!(io::stderr(), LeaveAlternateScreen);
        }
    }
}
