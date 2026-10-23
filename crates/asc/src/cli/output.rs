//! Output kinds, the colour policy and the stdout writer a command prints through (paged when
//! the pager rules say so).
#![allow(dead_code)] // Used by the commands of later units.

use std::io::{self, IsTerminal, Write};

use super::common::{Ctx, Result};
use super::pager::{Pager, PagerWant, resolve};

/// What a command prints, which decides display ids and colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    /// Text for people.
    Human,
    /// `--porcelain`, `-z`, `--format`/`--pretty`.
    Porcelain,
    /// Plumbing output.
    Plumbing,
    /// `--json`.
    Json,
}

/// git's `color.ui` and friends: `always`, `auto` (the default), `never`, booleans.
pub fn use_color(
    color_ui: Option<&str>,
    no_color: bool,
    stdout_is_tty: bool,
    term: Option<&str>,
) -> bool {
    match color_ui.map(str::to_ascii_lowercase).as_deref() {
        Some("always") => true,
        Some("never" | "false" | "no" | "off" | "0") => false,
        Some("true" | "yes" | "on" | "1" | "auto" | "") | None => {
            stdout_is_tty && !no_color && term != Some("dumb")
        }
        Some(_) => false,
    }
}

/// The stdout a command writes to.
pub struct Output {
    sink: Sink,
    color: bool,
}

enum Sink {
    Direct(io::BufWriter<io::Stdout>),
    Paged(Pager),
}

impl Output {
    /// Opens stdout for `command`, starting the pager when the rules say so.
    pub fn open(ctx: &Ctx, command: &str, want: PagerWant) -> Result<Self> {
        let tty = io::stdout().is_terminal();
        let color = use_color(
            ctx.config.get("color.ui").as_deref(),
            ctx.env.no_color,
            tty,
            std::env::var("TERM").ok().as_deref(),
        );
        let sink = match resolve(&ctx.env, &ctx.config, command, want, tty) {
            Some(line) => Sink::Paged(Pager::spawn(ctx, &line)?),
            None => Sink::Direct(io::BufWriter::new(io::stdout())),
        };
        Ok(Self { sink, color })
    }

    /// Whether to colour output.
    pub fn color(&self) -> bool {
        self.color
    }

    /// Flushes and, for a pager, waits for it to exit.
    pub fn finish(self) -> io::Result<()> {
        match self.sink {
            Sink::Direct(mut w) => w.flush(),
            Sink::Paged(p) => p.finish(),
        }
    }
}

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.sink {
            Sink::Direct(w) => w.write(buf),
            Sink::Paged(p) => p.stdin().write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.sink {
            Sink::Direct(w) => w.flush(),
            Sink::Paged(p) => p.stdin().flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_policy() {
        assert!(use_color(Some("always"), true, false, None));
        assert!(!use_color(Some("never"), false, true, None));
        assert!(use_color(None, false, true, Some("xterm")));
        assert!(
            !use_color(None, false, false, Some("xterm")),
            "not a terminal"
        );
        assert!(!use_color(None, true, true, Some("xterm")), "NO_COLOR");
        assert!(!use_color(Some("auto"), false, true, Some("dumb")));
        assert!(use_color(Some("true"), false, true, None));
        assert!(!use_color(Some("nonsense"), false, true, None));
    }
}
