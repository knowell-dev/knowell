//! Buffered stdout that treats a closed pipe (`know … | head`) as success.

use std::io::{self, BufWriter, Stdout, Write};

/// Line-oriented writer for command results.
pub(crate) struct Output {
    inner: BufWriter<Stdout>,
    closed: bool,
}

impl Output {
    pub(crate) fn stdout() -> Self {
        Self {
            inner: BufWriter::new(io::stdout()),
            closed: false,
        }
    }

    /// Writes one line. After the reader goes away, further output is
    /// silently dropped instead of failing the command.
    pub(crate) fn line(&mut self, text: impl AsRef<str>) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        let result = self
            .inner
            .write_all(text.as_ref().as_bytes())
            .and_then(|()| self.inner.write_all(b"\n"));
        self.absorb_broken_pipe(result)
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        let result = self.inner.flush();
        self.absorb_broken_pipe(result)
    }

    fn absorb_broken_pipe(&mut self, result: io::Result<()>) -> io::Result<()> {
        match result {
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => {
                self.closed = true;
                Ok(())
            }
            other => other,
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // Errors are reported by explicit `flush` calls; nothing useful to do here.
        let _ = self.flush();
    }
}
