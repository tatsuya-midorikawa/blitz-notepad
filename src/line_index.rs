use memchr::memchr2_iter;
use serde::{Deserialize, Serialize};

const LINE_ENDING_SAMPLE_LIMIT: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LineEnding {
    CrLf,
    Lf,
    Cr,
}

impl LineEnding {
    pub fn label(self) -> &'static str {
        match self {
            LineEnding::CrLf => "Windows (CRLF)",
            LineEnding::Lf => "Unix (LF)",
            LineEnding::Cr => "Macintosh (CR)",
        }
    }

    pub fn sequence(self) -> &'static str {
        match self {
            LineEnding::CrLf => "\r\n",
            LineEnding::Lf => "\n",
            LineEnding::Cr => "\r",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn build(bytes: &[u8]) -> Self {
        let mut starts = Vec::with_capacity(bytes.len().saturating_div(80).max(1));
        starts.push(0);

        let mut skip_lf_at = None;
        for newline_index in memchr2_iter(b'\r', b'\n', bytes) {
            if skip_lf_at == Some(newline_index) {
                skip_lf_at = None;
                continue;
            }

            if bytes[newline_index] == b'\r' && bytes.get(newline_index + 1) == Some(&b'\n') {
                starts.push(newline_index + 2);
                skip_lf_at = Some(newline_index + 1);
            } else {
                starts.push(newline_index + 1);
                skip_lf_at = None;
            }
        }

        Self { starts }
    }

    pub fn starts(&self) -> &[usize] {
        &self.starts
    }

    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    pub fn line_for_offset(&self, offset: usize) -> usize {
        self.starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1)
    }

    pub fn line_start(&self, zero_based_line: usize) -> Option<usize> {
        self.starts.get(zero_based_line).copied()
    }

    pub fn line_range(
        &self,
        zero_based_line: usize,
        document_len: usize,
    ) -> Option<std::ops::Range<usize>> {
        let start = self.line_start(zero_based_line)?;
        let end = self
            .line_start(zero_based_line + 1)
            .unwrap_or(document_len)
            .min(document_len);
        Some(start..end)
    }
}

pub fn detect_line_ending(bytes: &[u8]) -> LineEnding {
    let sample = &bytes[..bytes.len().min(LINE_ENDING_SAMPLE_LIMIT)];
    let mut crlf_count = 0usize;
    let mut lf_count = 0usize;
    let mut cr_count = 0usize;
    let mut index = 0usize;

    while index < sample.len() {
        match sample[index] {
            b'\r' if sample.get(index + 1) == Some(&b'\n') => {
                crlf_count += 1;
                index += 2;
            }
            b'\r' => {
                cr_count += 1;
                index += 1;
            }
            b'\n' => {
                lf_count += 1;
                index += 1;
            }
            _ => index += 1,
        }
    }

    if crlf_count >= lf_count && crlf_count >= cr_count && crlf_count > 0 {
        LineEnding::CrLf
    } else if lf_count >= cr_count && lf_count > 0 {
        LineEnding::Lf
    } else if cr_count > 0 {
        LineEnding::Cr
    } else {
        LineEnding::CrLf
    }
}

pub fn normalize_line_endings(text: &str, target: LineEnding) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(character) = chars.next() {
        match character {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                output.push_str(target.sequence());
            }
            '\n' => output.push_str(target.sequence()),
            _ => output.push(character),
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_line_starts_for_mixed_newlines() {
        let index = LineIndex::build(b"a\r\nb\nc\rd");
        assert_eq!(index.starts(), &[0, 3, 5, 7]);
        assert_eq!(index.line_for_offset(4), 1);
    }

    #[test]
    fn normalizes_line_endings() {
        assert_eq!(
            normalize_line_endings("a\rb\r\nc\n", LineEnding::Lf),
            "a\nb\nc\n"
        );
    }
}
