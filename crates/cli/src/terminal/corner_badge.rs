//! A right-aligned transient badge at the bottom-right of the transcript.
use crossterm::{
    cursor::{MoveTo, RestorePosition, SavePosition},
    queue,
    style::{Attribute, Print, SetAttribute},
};
use std::io::{self, Write};
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub(super) struct CornerBadge {
    row: Option<u16>,
    column: u16,
    width: usize,
    text: String,
}

impl CornerBadge {
    /// Paints `text` as a reverse-video badge one cell from the right edge.
    /// Repainting the same cells is intentional: a frame may have overwritten
    /// the row since the last loop iteration.
    pub fn show(&mut self, row: u16, text: &str) -> io::Result<()> {
        let (columns, _) = crate::terminal::size();
        let width = UnicodeWidthStr::width(text) + 2;
        if width + 1 > columns as usize {
            return self.clear();
        }
        let column = (columns as usize - width - 1) as u16;
        if self.row != Some(row) || self.column != column || self.text != text {
            self.clear()?;
        }
        let mut out = crate::terminal_output::stderr_locked();
        queue!(
            out,
            SavePosition,
            MoveTo(column, row),
            SetAttribute(Attribute::Reverse),
            Print(format!(" {text} ")),
            SetAttribute(Attribute::Reset),
            RestorePosition
        )?;
        out.flush()?;
        self.row = Some(row);
        self.column = column;
        self.width = width;
        self.text = text.to_owned();
        Ok(())
    }

    pub fn clear(&mut self) -> io::Result<()> {
        let Some(row) = self.row.take() else {
            return Ok(());
        };
        let mut out = crate::terminal_output::stderr_locked();
        queue!(
            out,
            SavePosition,
            MoveTo(self.column, row),
            Print(" ".repeat(self.width)),
            RestorePosition
        )?;
        out.flush()?;
        self.width = 0;
        self.text.clear();
        Ok(())
    }
}
