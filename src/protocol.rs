use crate::entries::{MAX_BODY, validate_body};
use anyhow::{Context, Result, ensure};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub(crate) struct Boundary(String);

impl Boundary {
    pub(crate) fn new(value: Option<String>) -> Result<Self> {
        let value = value.unwrap_or_else(|| Uuid::new_v4().simple().to_string());
        ensure!(
            (1..=70).contains(&value.len())
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte)),
            "boundary must be 1–70 ASCII letters, digits, hyphens or underscores"
        );
        Ok(Self(value))
    }

    pub(crate) fn value(&self) -> &str {
        &self.0
    }

    fn delimiter(&self) -> String {
        format!("--{}", self.0)
    }

    fn no_final_lf(&self) -> String {
        format!("nunc-no-final-lf:{}\n", self.0)
    }

    pub(crate) fn frame(&self, body: &str) -> Result<Vec<u8>> {
        validate_body(body)?;
        let delimiter = self.delimiter();
        let marker = self.no_final_lf();
        ensure!(
            !body.starts_with(&marker),
            "entry starts with the reserved FIFO marker; use nunc log"
        );
        ensure!(
            !body
                .split('\n')
                .any(|line| line.strip_suffix('\r').unwrap_or(line) == delimiter),
            "entry contains the session delimiter; use nunc log"
        );
        let mut frame = Vec::with_capacity(body.len() + marker.len() + delimiter.len() + 2);
        if !body.ends_with('\n') {
            frame.extend_from_slice(marker.as_bytes());
        }
        frame.extend_from_slice(body.as_bytes());
        if !body.ends_with('\n') {
            frame.push(b'\n');
        }
        frame.extend_from_slice(delimiter.as_bytes());
        frame.push(b'\n');
        Ok(frame)
    }
}

pub(crate) struct Parser {
    delimiter: String,
    marker: String,
    line: Vec<u8>,
    body: Vec<u8>,
    oversized: bool,
    long_line: bool,
}

impl Parser {
    pub(crate) fn new(boundary: &Boundary) -> Self {
        Self {
            delimiter: boundary.delimiter(),
            marker: boundary.no_final_lf(),
            line: Vec::new(),
            body: Vec::new(),
            oversized: false,
            long_line: false,
        }
    }

    fn limit(&self) -> usize {
        MAX_BODY + self.marker.len() + 1
    }

    pub(crate) fn push(&mut self, bytes: &[u8], mut record: impl FnMut(Result<String>)) {
        for part in bytes.split_inclusive(|byte| *byte == b'\n') {
            let ceiling = if self.oversized {
                self.delimiter.len() + 2
            } else {
                self.limit()
            };
            if self.line.len() + part.len() > ceiling {
                self.line.clear();
                self.oversized = true;
                self.long_line = true;
                self.body.clear();
            }
            if !self.long_line {
                self.line.extend_from_slice(part);
            }
            if !part.ends_with(b"\n") {
                continue;
            }

            let delimiter = !self.long_line
                && self.line.strip_suffix(b"\n").is_some_and(|line| {
                    line.strip_suffix(b"\r").unwrap_or(line) == self.delimiter.as_bytes()
                });
            if delimiter {
                record(self.finish());
            } else if !self.oversized {
                if self.body.len() + self.line.len() <= self.limit() {
                    self.body.extend_from_slice(&self.line);
                } else {
                    self.body.clear();
                    self.oversized = true;
                }
            }
            self.line.clear();
            self.long_line = false;
        }
    }

    fn finish(&mut self) -> Result<String> {
        let oversized = std::mem::take(&mut self.oversized);
        let bytes = std::mem::take(&mut self.body);
        ensure!(!oversized, "FIFO entry exceeds {MAX_BODY} bytes");
        let mut text = String::from_utf8(bytes).context("FIFO entry must be UTF-8")?;
        if text.starts_with(&self.marker) {
            text.drain(..self.marker.len());
            ensure!(text.ends_with('\n'), "incomplete FIFO transport marker");
            text.pop();
        }
        validate_body(&text)?;
        Ok(text)
    }

    pub(crate) fn end_stream(&mut self) -> bool {
        let incomplete = self.oversized || !self.line.is_empty() || !self.body.is_empty();
        self.line.clear();
        self.body.clear();
        self.oversized = false;
        self.long_line = false;
        incomplete
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_markdown_and_accepts_split_frames() -> Result<()> {
        let boundary = Boundary::new(Some("demo".into()))?;
        let mut parser = Parser::new(&boundary);
        let mut frames = Vec::new();
        let contents = ["first\r\n\n```\n--demo-ish\n```", "second\n", "third\r"];
        for body in contents {
            for byte in boundary.frame(body)? {
                parser.push(&[byte], |frame| frames.push(frame));
            }
        }
        parser.push(b"raw\r\n\n--demo\r\nnext\n--demo\n", |frame| {
            frames.push(frame)
        });
        assert_eq!(
            frames.into_iter().collect::<Result<Vec<_>>>()?,
            [contents[0], contents[1], contents[2], "raw\r\n\n", "next\n"]
        );
        assert!(!parser.end_stream());
        Ok(())
    }

    #[test]
    fn resynchronizes_after_bad_frames_and_discards_partial_eof() -> Result<()> {
        let boundary = Boundary::new(Some("demo".into()))?;
        assert!(Boundary::new(Some("bad\n".into())).is_err());
        assert!(boundary.frame("good\n--demo\r\nbad").is_err());
        let mut parser = Parser::new(&boundary);
        let mut frames = Vec::new();
        parser.push(&vec![b'a'; MAX_BODY + 256], |frame| frames.push(frame));
        parser.push(b"\n--demo\n--demo\n\xff\n--demo\nfirst\n--dem", |frame| {
            frames.push(frame)
        });
        assert!(parser.end_stream());
        parser.push(b"last\n--demo\n", |frame| frames.push(frame));
        assert_eq!(frames.len(), 4);
        assert!(frames[..3].iter().all(Result::is_err));
        assert_eq!(frames.pop().transpose()?.as_deref(), Some("last\n"));
        Ok(())
    }
}
