use std::ops::Range;
use std::thread;

use memchr::memchr2_iter;
use serde::{Deserialize, Serialize};

const LINE_ENDING_SAMPLE_LIMIT: usize = 4_096;
const PARALLEL_LINE_INDEX_MIN_BYTES: usize = 64 * 1024 * 1024;
const PARALLEL_LINE_INDEX_CHUNK_BYTES: usize = 64 * 1024 * 1024;

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
    indexed_len: usize,
    complete: bool,
}

impl LineIndex {
    pub fn build(bytes: &[u8]) -> Self {
        let mut starts = Vec::new();
        starts.push(0);
        starts.extend(newline_starts(0, bytes));

        Self {
            starts,
            indexed_len: bytes.len(),
            complete: true,
        }
    }

    pub fn build_parallel(bytes: &[u8]) -> Self {
        if bytes.len() < PARALLEL_LINE_INDEX_MIN_BYTES {
            return Self::build(bytes);
        }

        let available_workers = thread::available_parallelism()
            .map(|parallelism| parallelism.get())
            .unwrap_or(1);
        let target_workers = bytes.len().div_ceil(PARALLEL_LINE_INDEX_CHUNK_BYTES);
        let worker_count = available_workers.min(target_workers).max(1);
        if worker_count <= 1 {
            return Self::build(bytes);
        }

        build_parallel_with_chunk_len(bytes, bytes.len().div_ceil(worker_count))
    }

    pub fn build_prefix(bytes: &[u8], max_indexed_len: usize) -> Self {
        let indexed_len = safe_prefix_len(bytes, max_indexed_len.min(bytes.len()));
        let mut starts = Vec::new();
        starts.push(0);
        starts.extend(newline_starts(0, &bytes[..indexed_len]));

        Self {
            starts,
            indexed_len,
            complete: indexed_len == bytes.len(),
        }
    }

    pub(crate) fn replace_with_starts(
        starts: Vec<usize>,
        indexed_len: usize,
        complete: bool,
    ) -> Self {
        debug_assert!(!starts.is_empty());
        debug_assert_eq!(starts[0], 0);
        debug_assert!(starts.windows(2).all(|window| window[0] < window[1]));
        Self {
            starts,
            indexed_len,
            complete,
        }
    }

    pub fn starts(&self) -> &[usize] {
        &self.starts
    }

    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    pub fn indexed_len(&self) -> usize {
        self.indexed_len
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    pub fn extend(&mut self, bytes: &[u8], additional_len: usize) {
        if self.complete {
            return;
        }

        let target_len = self
            .indexed_len
            .saturating_add(additional_len)
            .min(bytes.len());
        let next_indexed_len = safe_prefix_len(bytes, target_len);
        if next_indexed_len <= self.indexed_len && target_len < bytes.len() {
            return;
        }

        let end = if target_len == bytes.len() {
            bytes.len()
        } else {
            next_indexed_len
        };
        if end > self.indexed_len {
            self.starts.extend(newline_starts(
                self.indexed_len,
                &bytes[self.indexed_len..end],
            ));
            self.indexed_len = end;
        }
        self.complete = self.indexed_len == bytes.len();
    }

    pub fn extend_from_chunk(&mut self, chunk: &[u8], document_len: usize) {
        if self.complete || chunk.is_empty() {
            return;
        }

        let mut scan_len = chunk
            .len()
            .min(document_len.saturating_sub(self.indexed_len));
        if self.indexed_len + scan_len < document_len
            && scan_len > 0
            && chunk[scan_len - 1] == b'\r'
        {
            scan_len -= 1;
        }
        if scan_len == 0 {
            return;
        }

        self.starts
            .extend(newline_starts(self.indexed_len, &chunk[..scan_len]));
        self.indexed_len += scan_len;
        self.complete = self.indexed_len == document_len;
    }

    pub fn line_for_offset(&self, offset: usize) -> usize {
        self.starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1)
    }

    pub fn line_start(&self, zero_based_line: usize) -> Option<usize> {
        self.starts.get(zero_based_line).copied()
    }

    pub fn line_range(&self, zero_based_line: usize, document_len: usize) -> Option<Range<usize>> {
        let start = self.line_start(zero_based_line)?;
        let end = self
            .line_start(zero_based_line + 1)
            .unwrap_or(if self.complete {
                document_len
            } else {
                self.indexed_len
            })
            .min(document_len);
        Some(start..end)
    }
}

fn safe_prefix_len(bytes: &[u8], requested_len: usize) -> usize {
    if requested_len >= bytes.len() {
        bytes.len()
    } else if requested_len > 0 && bytes[requested_len - 1] == b'\r' {
        requested_len - 1
    } else {
        requested_len
    }
}

fn newline_starts(base_offset: usize, bytes: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut skip_lf_at = None;
    append_newline_starts(
        &mut starts,
        base_offset,
        bytes,
        memchr2_iter(b'\r', b'\n', bytes).map(|index| base_offset + index),
        &mut skip_lf_at,
    );
    starts
}

fn build_parallel_with_chunk_len(bytes: &[u8], chunk_len: usize) -> LineIndex {
    if bytes.is_empty() || chunk_len == 0 || chunk_len >= bytes.len() {
        return LineIndex::build(bytes);
    }

    let ranges = (0..bytes.len())
        .step_by(chunk_len)
        .map(|start| start..(start + chunk_len).min(bytes.len()))
        .collect::<Vec<_>>();
    let scan_results = thread::scope(|scope| {
        let handles = ranges
            .iter()
            .cloned()
            .map(|range| scope.spawn(move || newline_positions(range.start, &bytes[range])))
            .collect::<Vec<_>>();
        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            match handle.join() {
                Ok(positions) => results.push(positions),
                Err(_) => return None,
            }
        }
        Some(results)
    });
    let Some(position_chunks) = scan_results else {
        return LineIndex::build(bytes);
    };

    let newline_count = position_chunks.iter().map(Vec::len).sum::<usize>();
    let mut starts = Vec::with_capacity(newline_count.saturating_add(1));
    let mut skip_lf_at = None;
    starts.push(0);
    for positions in position_chunks {
        append_newline_starts(&mut starts, 0, bytes, positions, &mut skip_lf_at);
    }

    LineIndex {
        starts,
        indexed_len: bytes.len(),
        complete: true,
    }
}

fn newline_positions(base_offset: usize, bytes: &[u8]) -> Vec<usize> {
    memchr2_iter(b'\r', b'\n', bytes)
        .map(|index| base_offset + index)
        .collect()
}

fn append_newline_starts(
    starts: &mut Vec<usize>,
    base_offset: usize,
    bytes: &[u8],
    newline_positions: impl IntoIterator<Item = usize>,
    skip_lf_at: &mut Option<usize>,
) {
    for newline_index in newline_positions {
        if *skip_lf_at == Some(newline_index) {
            *skip_lf_at = None;
            continue;
        }

        debug_assert!(newline_index >= base_offset);
        let local_index = newline_index - base_offset;
        if bytes[local_index] == b'\r' && bytes.get(local_index + 1) == Some(&b'\n') {
            starts.push(newline_index + 2);
            *skip_lf_at = Some(newline_index + 1);
        } else {
            starts.push(newline_index + 1);
            *skip_lf_at = None;
        }
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
    fn parallel_build_matches_sequential_with_chunk_boundary_crlf() {
        let bytes = b"aa\r\nbb\ncc\rdd\r\nee";

        assert_eq!(
            build_parallel_with_chunk_len(bytes, 3),
            LineIndex::build(bytes)
        );
    }

    #[test]
    fn parallel_build_matches_sequential_with_absolute_chunk_offsets() {
        let bytes = b"abc\ndef\nghi\r\njkl\rmno\n";

        assert_eq!(
            build_parallel_with_chunk_len(bytes, 5),
            LineIndex::build(bytes)
        );
    }

    #[test]
    fn parallel_build_matches_sequential_for_many_chunks() {
        let mut bytes = Vec::new();
        for index in 0..512 {
            bytes.extend_from_slice(format!("line {index}\r\n").as_bytes());
        }

        assert_eq!(
            build_parallel_with_chunk_len(&bytes, 17),
            LineIndex::build(&bytes)
        );
    }

    #[test]
    fn prefix_index_does_not_scan_the_whole_buffer() {
        let index = LineIndex::build_prefix(b"a\nb\nc\nd", 4);

        assert!(!index.is_complete());
        assert_eq!(index.indexed_len(), 4);
        assert_eq!(index.starts(), &[0, 2, 4]);
        assert_eq!(index.line_range(2, 7), Some(4..4));
    }

    #[test]
    fn prefix_index_extends_in_chunks() {
        let bytes = b"a\nb\nc\nd";
        let mut index = LineIndex::build_prefix(bytes, 2);

        index.extend(bytes, 5);

        assert!(index.is_complete());
        assert_eq!(index.starts(), &[0, 2, 4, 6]);
        assert_eq!(index.line_range(3, bytes.len()), Some(6..7));
    }

    #[test]
    fn normalizes_line_endings() {
        assert_eq!(
            normalize_line_endings("a\rb\r\nc\n", LineEnding::Lf),
            "a\nb\nc\n"
        );
    }
}
