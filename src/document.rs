use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use memmap2::Mmap;

use crate::encoding::{decode_to_utf8, detect_encoding, encode_from_utf8, TextEncoding};
use crate::error::io_path;
use crate::line_index::{detect_line_ending, normalize_line_endings, LineEnding, LineIndex};
use crate::piece_table::{PieceTable, SourceBytes};
use crate::{BlitzError, Result};

pub const MMAP_THRESHOLD_BYTES: u64 = 50 * 1024 * 1024;
const UNDO_LIMIT: usize = 100;
const TAB_WIDTH: usize = 8;
const MAX_VISIBLE_LINE_BYTES: usize = 16 * 1024;
const MAX_COLUMN_SCAN_BYTES: usize = 64 * 1024;
const INITIAL_MMAP_LINE_INDEX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadMode {
    Heap,
    MemoryMapped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaretPosition {
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisibleLine {
    pub number: usize,
    pub byte_range: Range<usize>,
    pub text: String,
}

#[derive(Clone, Debug)]
struct UndoSnapshot {
    inverse_edits: Vec<TextEdit>,
    change_generation: u64,
}

type PendingLineIndex = Arc<Mutex<Option<LineIndex>>>;

#[derive(Clone, Debug)]
pub struct Document {
    path: Option<PathBuf>,
    buffer: PieceTable,
    line_index: RefCell<LineIndex>,
    pending_line_index: RefCell<Option<PendingLineIndex>>,
    encoding: TextEncoding,
    line_ending: LineEnding,
    dirty: bool,
    load_mode: LoadMode,
    original_file_len: u64,
    change_generation: u64,
    saved_generation: Option<u64>,
    undo_stack: Vec<UndoSnapshot>,
}

impl Document {
    pub fn new_untitled() -> Self {
        Self {
            path: None,
            buffer: PieceTable::new(),
            line_index: RefCell::new(LineIndex::build(&[])),
            pending_line_index: RefCell::new(None),
            encoding: TextEncoding::Utf8,
            line_ending: LineEnding::CrLf,
            dirty: false,
            load_mode: LoadMode::Heap,
            original_file_len: 0,
            change_generation: 0,
            saved_generation: Some(0),
            undo_stack: Vec::new(),
        }
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let loaded = load_source(path)?;
        let encoding = detect_encoding(loaded.source.as_slice());

        let (buffer, line_index, pending_line_index, line_ending) = match encoding {
            TextEncoding::Utf8 | TextEncoding::Utf8Bom => {
                let body_start = encoding.bom_len(loaded.source.as_slice());
                let body_len = loaded.source.len().saturating_sub(body_start);
                let body = &loaded.source.as_slice()[body_start..body_start + body_len];
                let (line_index, pending_line_index) =
                    build_open_line_index(loaded.source.clone(), body_start, body_len, loaded.mode);
                (
                    PieceTable::from_source(loaded.source.clone(), body_start, body_len)?,
                    line_index,
                    pending_line_index,
                    detect_line_ending(body),
                )
            }
            TextEncoding::Utf16Le | TextEncoding::Utf16Be | TextEncoding::Ansi => {
                let decoded = decode_to_utf8(loaded.source.as_slice(), encoding)?;
                let bytes = decoded.into_bytes();
                let line_index = LineIndex::build(&bytes);
                let line_ending = detect_line_ending(&bytes);
                let body_len = bytes.len();
                (
                    PieceTable::from_source(SourceBytes::from_vec(bytes), 0, body_len)?,
                    line_index,
                    None,
                    line_ending,
                )
            }
        };

        Ok(Self {
            path: Some(path.to_path_buf()),
            buffer,
            line_index: RefCell::new(line_index),
            pending_line_index: RefCell::new(pending_line_index),
            encoding,
            line_ending,
            dirty: false,
            load_mode: loaded.mode,
            original_file_len: loaded.original_file_len,
            change_generation: 0,
            saved_generation: Some(0),
            undo_stack: Vec::new(),
        })
    }

    pub fn file_name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("Untitled")
            .to_owned()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn encoding(&self) -> TextEncoding {
        self.encoding
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn load_mode(&self) -> LoadMode {
        self.load_mode
    }

    pub fn original_file_len(&self) -> u64 {
        self.original_file_len
    }

    pub(crate) fn change_generation(&self) -> u64 {
        self.change_generation
    }

    pub fn line_count(&self) -> usize {
        self.refresh_line_index();
        self.line_count_snapshot()
    }

    pub(crate) fn line_count_snapshot(&self) -> usize {
        self.line_index.borrow().line_count()
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn undo_available(&self) -> bool {
        self.can_undo()
    }

    pub fn text_lossy(&self) -> String {
        self.buffer.to_text_lossy()
    }

    pub fn text(&self) -> String {
        self.text_lossy()
    }

    pub fn text_range_lossy(&self, range: Range<usize>) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.buffer.bytes_range(range)?).into_owned())
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.buffer.collect_bytes()
    }

    pub(crate) fn for_each_chunk<E>(
        &self,
        visit: impl FnMut(&[u8]) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        self.buffer.for_each_chunk(visit)
    }

    pub(crate) fn for_each_chunk_rev<E>(
        &self,
        visit: impl FnMut(usize, &[u8]) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        self.buffer.for_each_chunk_rev(visit)
    }

    pub fn insert_text(&mut self, byte_offset: usize, text: &str) -> Result<()> {
        self.ensure_char_boundary(byte_offset)?;
        if text.is_empty() {
            return Ok(());
        }

        let previous_generation = self.change_generation;
        self.replace_range_without_undo(byte_offset..byte_offset, text)?;
        self.commit_successful_edit(
            vec![TextEdit {
                range: byte_offset..byte_offset + text.len(),
                replacement: String::new(),
            }],
            previous_generation,
        );
        Ok(())
    }

    pub fn delete_range(&mut self, range: Range<usize>) -> Result<()> {
        self.ensure_range(&range)?;
        if range.is_empty() {
            return Ok(());
        }

        let replacement = self.validated_text_range(range.clone())?;
        let previous_generation = self.change_generation;
        let start = range.start;
        self.replace_range_without_undo(range, "")?;
        self.commit_successful_edit(
            vec![TextEdit {
                range: start..start,
                replacement,
            }],
            previous_generation,
        );
        Ok(())
    }

    pub fn replace_range(&mut self, range: Range<usize>, text: &str) -> Result<()> {
        self.ensure_range(&range)?;
        if range.is_empty() && text.is_empty() {
            return Ok(());
        }

        let replacement = self.validated_text_range(range.clone())?;
        let previous_generation = self.change_generation;
        let start = range.start;
        self.replace_range_without_undo(range, text)?;
        self.commit_successful_edit(
            vec![TextEdit {
                range: start..start + text.len(),
                replacement,
            }],
            previous_generation,
        );
        Ok(())
    }

    pub fn apply_edits_from_end(&mut self, edits: &[TextEdit]) -> Result<()> {
        if edits.is_empty() {
            return Ok(());
        }

        let mut edit_infos = edits
            .iter()
            .map(|edit| {
                self.ensure_range(&edit.range)?;
                Ok(EditInfo {
                    range: edit.range.clone(),
                    replacement_len: edit.replacement.len(),
                    original_text: self.validated_text_range(edit.range.clone())?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        edit_infos.sort_by(|left, right| left.range.start.cmp(&right.range.start));
        reject_overlapping_edits(&edit_infos, self.len())?;

        let previous_generation = self.change_generation;
        let mut sorted = edits.to_vec();
        sorted.sort_by(|left, right| right.range.start.cmp(&left.range.start));
        for edit in sorted {
            self.replace_range_without_undo(edit.range, &edit.replacement)?;
        }
        self.commit_successful_edit(inverse_edits(edit_infos), previous_generation);
        Ok(())
    }

    pub fn undo(&mut self) -> Result<bool> {
        let Some(snapshot) = self.undo_stack.pop() else {
            return Ok(false);
        };

        let mut sorted = snapshot.inverse_edits;
        sorted.sort_by(|left, right| right.range.start.cmp(&left.range.start));
        for edit in sorted {
            self.replace_range_without_undo(edit.range, &edit.replacement)?;
        }
        self.change_generation = snapshot.change_generation;
        self.sync_dirty_flag();
        Ok(true)
    }

    pub fn caret_position(&self, byte_offset: usize) -> Result<CaretPosition> {
        if byte_offset > self.len() {
            return Err(BlitzError::InvalidRange {
                start: byte_offset,
                end: byte_offset,
                len: self.len(),
            });
        }
        self.refresh_line_index();
        let (line, line_start) = {
            let line_index = self.line_index.borrow();
            let line = line_index.line_for_offset(byte_offset);
            let line_start = line_index.line_start(line).unwrap_or(0);
            (line, line_start)
        };
        let column = if byte_offset - line_start <= MAX_COLUMN_SCAN_BYTES {
            let before_caret = self.validated_text_range(line_start..byte_offset)?;
            visual_column(&before_caret)
        } else {
            byte_offset - line_start + 1
        };
        Ok(CaretPosition {
            line: line + 1,
            column,
        })
    }

    pub fn visible_lines(&self, first_line: usize, max_lines: usize) -> Vec<VisibleLine> {
        self.visible_lines_at(first_line, max_lines, 0)
    }

    pub fn visible_lines_at(
        &self,
        first_line: usize,
        max_lines: usize,
        horizontal_offset: usize,
    ) -> Vec<VisibleLine> {
        (first_line..first_line.saturating_add(max_lines))
            .filter_map(|zero_based_line| {
                let line_range = self.line_range(zero_based_line)?;
                let content_range = self.trimmed_line_range(line_range).ok()?;
                let scrolled_start = content_range
                    .start
                    .saturating_add(horizontal_offset)
                    .min(content_range.end);
                let scrolled_start = self
                    .floor_char_boundary(scrolled_start, content_range.start)
                    .ok()?;
                let byte_range = self.capped_range(scrolled_start..content_range.end).ok()?;
                let text = self.text_range_lossy(byte_range.clone()).ok()?;
                Some(VisibleLine {
                    number: zero_based_line + 1,
                    byte_range,
                    text,
                })
            })
            .collect()
    }

    pub fn line_range(&self, zero_based_line: usize) -> Option<Range<usize>> {
        self.refresh_line_index();
        self.line_index
            .borrow()
            .line_range(zero_based_line, self.len())
    }

    pub fn line_start(&self, zero_based_line: usize) -> Option<usize> {
        self.refresh_line_index();
        self.line_index.borrow().line_start(zero_based_line)
    }

    pub fn line_for_offset(&self, byte_offset: usize) -> Result<usize> {
        self.ensure_char_boundary(byte_offset)?;
        self.refresh_line_index();
        Ok(self.line_index.borrow().line_for_offset(byte_offset))
    }

    pub fn line_content_range_for_offset(&self, byte_offset: usize) -> Result<Range<usize>> {
        let line = self.line_for_offset(byte_offset)?;
        let range = self.line_range(line).unwrap_or(self.len()..self.len());
        self.trimmed_line_range(range)
    }

    pub fn previous_char_offset(&self, byte_offset: usize) -> Result<Option<usize>> {
        self.ensure_char_boundary(byte_offset)?;
        if byte_offset == 0 {
            return Ok(None);
        }

        let mut offset = byte_offset - 1;
        while offset > 0 && self.byte_at(offset)?.is_some_and(is_utf8_continuation) {
            offset -= 1;
        }
        self.ensure_char_boundary(offset)?;
        Ok(Some(offset))
    }

    pub fn next_char_offset(&self, byte_offset: usize) -> Result<Option<usize>> {
        self.ensure_char_boundary(byte_offset)?;
        let Some(first_byte) = self.byte_at(byte_offset)? else {
            return Ok(None);
        };
        let width = utf8_char_width(first_byte)?;
        let next_offset = byte_offset + width;
        if next_offset > self.len() {
            return Err(BlitzError::Encoding(
                "document contains a truncated UTF-8 character".to_owned(),
            ));
        }
        self.validated_text_range(byte_offset..next_offset)?;
        Ok(Some(next_offset))
    }

    pub fn char_column_for_offset(&self, byte_offset: usize) -> Result<usize> {
        let line = self.line_for_offset(byte_offset)?;
        let line_start = self.line_start(line).unwrap_or(0);
        if byte_offset - line_start > MAX_COLUMN_SCAN_BYTES {
            return Ok(byte_offset - line_start);
        }
        Ok(self
            .validated_text_range(line_start..byte_offset)?
            .chars()
            .count())
    }

    pub fn offset_for_char_column(&self, zero_based_line: usize, column: usize) -> Result<usize> {
        let Some(line_range) = self.line_range(zero_based_line) else {
            return Ok(self.len());
        };
        let content_range = self.trimmed_line_range(line_range)?;
        if content_range.len() > MAX_COLUMN_SCAN_BYTES {
            let sample_end = self.floor_char_boundary(
                content_range.start + MAX_COLUMN_SCAN_BYTES,
                content_range.start,
            )?;
            let sample = self.validated_text_range(content_range.start..sample_end)?;
            if let Some((index, _)) = sample.char_indices().nth(column) {
                return Ok(content_range.start + index);
            }

            let approximate = content_range
                .start
                .saturating_add(column)
                .min(content_range.end);
            return self.floor_char_boundary(approximate, content_range.start);
        }

        let text = self.validated_text_range(content_range.clone())?;
        Ok(text
            .char_indices()
            .nth(column)
            .map(|(index, _)| content_range.start + index)
            .unwrap_or(content_range.end))
    }

    pub fn save(&mut self) -> Result<()> {
        let path = self.path.clone().ok_or(BlitzError::MissingSavePath)?;
        let saved_len = self.write_to_path(&path, self.encoding, self.line_ending)?;
        self.mark_saved_generation(self.change_generation, saved_len);
        Ok(())
    }

    pub(crate) fn save_snapshot_to_path(
        &self,
        path: &Path,
        encoding: TextEncoding,
        line_ending: LineEnding,
    ) -> Result<u64> {
        self.write_to_path(path, encoding, line_ending)
    }

    pub(crate) fn mark_saved_generation(&mut self, generation: u64, saved_len: u64) -> bool {
        if self.change_generation != generation {
            return false;
        }

        self.original_file_len = saved_len;
        self.saved_generation = Some(generation);
        self.sync_dirty_flag();
        true
    }

    pub fn save_as(
        &mut self,
        path: impl AsRef<Path>,
        encoding: TextEncoding,
        line_ending: LineEnding,
    ) -> Result<()> {
        let path = path.as_ref();
        let saved_len = self.write_to_path(path, encoding, line_ending)?;
        self.path = Some(path.to_path_buf());
        self.encoding = encoding;
        self.line_ending = line_ending;
        self.load_mode = LoadMode::Heap;
        self.original_file_len = saved_len;
        self.saved_generation = Some(self.change_generation);
        self.sync_dirty_flag();
        Ok(())
    }

    pub fn set_line_ending(&mut self, line_ending: LineEnding) {
        if self.line_ending != line_ending {
            self.line_ending = line_ending;
            self.change_generation = self.change_generation.saturating_add(1);
            self.sync_dirty_flag();
        }
    }

    fn commit_successful_edit(&mut self, inverse_edits: Vec<TextEdit>, previous_generation: u64) {
        self.undo_stack.push(UndoSnapshot {
            inverse_edits,
            change_generation: previous_generation,
        });
        if self.undo_stack.len() > UNDO_LIMIT {
            self.undo_stack.remove(0);
        }
        self.change_generation = self.change_generation.saturating_add(1);
        self.sync_dirty_flag();
    }

    fn sync_dirty_flag(&mut self) {
        self.dirty = self.saved_generation != Some(self.change_generation);
    }

    pub(crate) fn refresh_line_index(&self) {
        let Some(pending) = self.pending_line_index.borrow().clone() else {
            return;
        };
        let Ok(mut completed) = pending.try_lock() else {
            return;
        };
        let Some(line_index) = completed.take() else {
            return;
        };

        *self.line_index.borrow_mut() = line_index;
        *self.pending_line_index.borrow_mut() = None;
    }

    fn replace_range_without_undo(&mut self, range: Range<usize>, text: &str) -> Result<()> {
        self.ensure_line_index_covers_offset(range.end)?;
        self.pending_line_index.get_mut().take();
        let previous_buffer = self.buffer.clone();
        self.buffer.replace_range(range.clone(), text)?;
        self.update_line_index_after_replace(&previous_buffer, range, text)
    }

    pub(crate) fn ensure_line_index_covers_offset(&self, byte_offset: usize) -> Result<()> {
        self.refresh_line_index();
        loop {
            let (indexed_len, complete) = {
                let line_index = self.line_index.borrow();
                (line_index.indexed_len(), line_index.is_complete())
            };
            if complete || byte_offset <= indexed_len {
                return Ok(());
            }

            let next_len = indexed_len
                .saturating_add(INITIAL_MMAP_LINE_INDEX_BYTES)
                .min(self.buffer.len());
            if next_len <= indexed_len {
                return Ok(());
            }

            self.extend_line_index_range(indexed_len..next_len)?;
        }
    }

    pub(crate) fn line_index_covers_offset(&self, byte_offset: usize) -> bool {
        self.refresh_line_index();
        let line_index = self.line_index.borrow();
        line_index.is_complete() || byte_offset <= line_index.indexed_len()
    }

    pub(crate) fn extend_line_index_towards_offset(
        &self,
        byte_offset: usize,
        max_bytes: usize,
    ) -> Result<bool> {
        self.refresh_line_index();
        let (indexed_len, complete) = {
            let line_index = self.line_index.borrow();
            (line_index.indexed_len(), line_index.is_complete())
        };
        if complete || byte_offset <= indexed_len {
            return Ok(true);
        }

        let next_len = indexed_len
            .saturating_add(max_bytes.max(1))
            .min(byte_offset)
            .min(self.buffer.len());
        if next_len > indexed_len {
            self.extend_line_index_range(indexed_len..next_len)?;
        }

        Ok(self.line_index_covers_offset(byte_offset))
    }

    fn extend_line_index_range(&self, range: Range<usize>) -> Result<()> {
        let document_len = self.buffer.len();
        let mut line_index = self.line_index.borrow_mut();
        self.buffer
            .for_each_chunk_range(range, |chunk_start, chunk| {
                let indexed_len = line_index.indexed_len();
                let consumed_from_chunk = if indexed_len < chunk_start && !chunk.is_empty() {
                    let bridge_start = indexed_len;
                    let bridge_end = chunk_start + 1;
                    if bridge_start < bridge_end {
                        if let Ok(bridge) = self.buffer.bytes_range(bridge_start..bridge_end) {
                            line_index.extend_from_chunk(&bridge, document_len);
                        }
                    }
                    line_index.indexed_len().saturating_sub(chunk_start)
                } else {
                    indexed_len.saturating_sub(chunk_start)
                };

                if consumed_from_chunk < chunk.len() {
                    line_index.extend_from_chunk(&chunk[consumed_from_chunk..], document_len);
                }
            })
    }

    fn update_line_index_after_replace(
        &mut self,
        previous_buffer: &PieceTable,
        range: Range<usize>,
        text: &str,
    ) -> Result<()> {
        let old_len = previous_buffer.len();
        let current_index = self.line_index.get_mut();
        let rebuild_start_line = current_index.line_for_offset(range.start);
        let rebuild_start = current_index.line_start(rebuild_start_line).unwrap_or(0);
        let rebuild_end_line = if range.end == old_len {
            current_index.line_count().saturating_sub(1)
        } else {
            current_index.line_for_offset(range.end)
        };
        let rebuild_end = current_index
            .line_start(rebuild_end_line + 1)
            .unwrap_or(old_len);

        let prefix = previous_buffer.bytes_range(rebuild_start..range.start)?;
        let suffix = previous_buffer.bytes_range(range.end..rebuild_end)?;
        let mut segment = Vec::with_capacity(prefix.len() + text.len() + suffix.len());
        segment.extend_from_slice(&prefix);
        segment.extend_from_slice(text.as_bytes());
        segment.extend_from_slice(&suffix);

        let segment_index = LineIndex::build(&segment);
        let segment_len = segment.len();
        let delta = text.len() as isize - (range.end - range.start) as isize;
        let old_starts = current_index.starts().to_vec();
        let suffix_start_index = old_starts.partition_point(|start| *start < rebuild_end);
        let mut starts = Vec::with_capacity(old_starts.len() + segment_index.line_count());

        starts.extend_from_slice(&old_starts[..=rebuild_start_line]);
        for relative_start in segment_index.starts().iter().copied().skip(1) {
            if rebuild_end < old_len && relative_start == segment_len {
                continue;
            }
            starts.push(rebuild_start + relative_start);
        }
        if rebuild_end < old_len {
            starts.extend(
                old_starts[suffix_start_index..]
                    .iter()
                    .copied()
                    .map(|start| offset_with_delta(start, delta)),
            );
        }
        starts.sort_unstable();
        starts.dedup();
        if starts.first().copied() != Some(0) {
            starts.insert(0, 0);
        }
        *current_index = LineIndex::replace_with_starts(starts, self.buffer.len(), true);
        Ok(())
    }

    fn trimmed_line_range(&self, range: Range<usize>) -> Result<Range<usize>> {
        let mut end = range.end;
        if end > range.start {
            match self.byte_at(end - 1)? {
                Some(b'\n') => {
                    end -= 1;
                    if end > range.start && self.byte_at(end - 1)? == Some(b'\r') {
                        end -= 1;
                    }
                }
                Some(b'\r') => end -= 1,
                _ => {}
            }
        }
        Ok(range.start..end)
    }

    fn capped_range(&self, range: Range<usize>) -> Result<Range<usize>> {
        if range.len() <= MAX_VISIBLE_LINE_BYTES {
            return Ok(range);
        }
        Ok(range.start
            ..self.floor_char_boundary(range.start + MAX_VISIBLE_LINE_BYTES, range.start)?)
    }

    fn floor_char_boundary(&self, mut offset: usize, lower_bound: usize) -> Result<usize> {
        while offset > lower_bound {
            match self.byte_at(offset)? {
                None => return Ok(offset),
                Some(byte) if !is_utf8_continuation(byte) => return Ok(offset),
                _ => offset -= 1,
            }
        }
        Ok(lower_bound)
    }

    fn validated_text_range(&self, range: Range<usize>) -> Result<String> {
        let bytes = self.buffer.bytes_range(range)?;
        std::str::from_utf8(&bytes)
            .map(str::to_owned)
            .map_err(|error| BlitzError::Encoding(format!("document is not valid UTF-8: {error}")))
    }

    fn ensure_range(&self, range: &Range<usize>) -> Result<()> {
        if range.start > range.end || range.end > self.len() {
            return Err(BlitzError::InvalidRange {
                start: range.start,
                end: range.end,
                len: self.len(),
            });
        }
        self.ensure_char_boundary(range.start)?;
        self.ensure_char_boundary(range.end)
    }

    fn ensure_char_boundary(&self, offset: usize) -> Result<()> {
        if offset > self.len() {
            return Err(BlitzError::InvalidRange {
                start: offset,
                end: offset,
                len: self.len(),
            });
        }
        if offset == self.len()
            || self
                .byte_at(offset)?
                .is_some_and(|byte| !is_utf8_continuation(byte))
        {
            Ok(())
        } else {
            Err(BlitzError::InvalidCharBoundary { offset })
        }
    }

    fn byte_at(&self, offset: usize) -> Result<Option<u8>> {
        self.buffer.byte_at(offset)
    }

    fn write_to_path(
        &self,
        path: &Path,
        encoding: TextEncoding,
        line_ending: LineEnding,
    ) -> Result<u64> {
        let temporary_path = temporary_save_path(path);
        let saved_len = self.write_temporary_file(&temporary_path, encoding, line_ending)?;
        fs::rename(&temporary_path, path)
            .or_else(|rename_error| {
                if path.exists() {
                    fs::remove_file(path)?;
                    fs::rename(&temporary_path, path)
                } else {
                    Err(rename_error)
                }
            })
            .map_err(|error| io_path(path, error))?;
        Ok(saved_len)
    }

    fn write_temporary_file(
        &self,
        temporary_path: &Path,
        encoding: TextEncoding,
        line_ending: LineEnding,
    ) -> Result<u64> {
        let file = File::create(temporary_path).map_err(|error| io_path(temporary_path, error))?;
        let mut writer = BufWriter::new(file);
        let saved_len = match encoding {
            TextEncoding::Utf8 | TextEncoding::Utf8Bom => self
                .write_utf8_stream(&mut writer, encoding, line_ending)
                .map_err(|error| io_path(temporary_path, error))?,
            TextEncoding::Utf16Le | TextEncoding::Utf16Be | TextEncoding::Ansi => {
                let normalized = normalize_line_endings(&self.text_lossy(), line_ending);
                let bytes = encode_from_utf8(&normalized, encoding)?;
                writer
                    .write_all(&bytes)
                    .map_err(|error| io_path(temporary_path, error))?;
                bytes.len() as u64
            }
        };
        writer
            .flush()
            .map_err(|error| io_path(temporary_path, error))?;
        Ok(saved_len)
    }

    fn write_utf8_stream<W: Write>(
        &self,
        writer: &mut W,
        encoding: TextEncoding,
        line_ending: LineEnding,
    ) -> std::io::Result<u64> {
        let mut written = 0u64;
        if encoding == TextEncoding::Utf8Bom {
            writer.write_all(b"\xEF\xBB\xBF")?;
            written += 3;
        }

        let mut pending_cr = false;
        self.buffer.for_each_chunk(|chunk| {
            write_normalized_utf8_chunk(writer, chunk, line_ending, &mut pending_cr, &mut written)
        })?;
        if pending_cr {
            let sequence = line_ending.sequence().as_bytes();
            writer.write_all(sequence)?;
            written += sequence.len() as u64;
        }
        Ok(written)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextEdit {
    pub range: Range<usize>,
    pub replacement: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EditInfo {
    range: Range<usize>,
    replacement_len: usize,
    original_text: String,
}

fn reject_overlapping_edits(edits: &[EditInfo], document_len: usize) -> Result<()> {
    for window in edits.windows(2) {
        if window[0].range.end > window[1].range.start {
            return Err(BlitzError::InvalidRange {
                start: window[1].range.start,
                end: window[0].range.end,
                len: document_len,
            });
        }
    }
    Ok(())
}

fn inverse_edits(edits: Vec<EditInfo>) -> Vec<TextEdit> {
    let mut cumulative_delta = 0isize;
    edits
        .into_iter()
        .map(|edit| {
            let start = offset_with_delta(edit.range.start, cumulative_delta);
            let replacement_len = edit.replacement_len;
            cumulative_delta += replacement_len as isize - edit.range.len() as isize;
            TextEdit {
                range: start..start + replacement_len,
                replacement: edit.original_text,
            }
        })
        .collect()
}

fn offset_with_delta(offset: usize, delta: isize) -> usize {
    if delta >= 0 {
        offset + delta as usize
    } else {
        offset - delta.unsigned_abs()
    }
}

fn is_utf8_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

fn utf8_char_width(first_byte: u8) -> Result<usize> {
    match first_byte {
        0x00..=0x7f => Ok(1),
        0xc2..=0xdf => Ok(2),
        0xe0..=0xef => Ok(3),
        0xf0..=0xf4 => Ok(4),
        _ => Err(BlitzError::Encoding(
            "document contains an invalid UTF-8 leading byte".to_owned(),
        )),
    }
}

#[derive(Debug)]
struct LoadedSource {
    source: SourceBytes,
    mode: LoadMode,
    original_file_len: u64,
}

fn load_source(path: &Path) -> Result<LoadedSource> {
    let metadata = fs::metadata(path).map_err(|error| io_path(path, error))?;
    if metadata.len() >= MMAP_THRESHOLD_BYTES && metadata.len() > 0 {
        let file = File::open(path).map_err(|error| io_path(path, error))?;
        let mmap = unsafe { Mmap::map(&file) }.map_err(|error| io_path(path, error))?;
        Ok(LoadedSource {
            source: SourceBytes::from_mmap(mmap),
            mode: LoadMode::MemoryMapped,
            original_file_len: metadata.len(),
        })
    } else {
        let bytes = fs::read(path).map_err(|error| io_path(path, error))?;
        Ok(LoadedSource {
            source: SourceBytes::from_vec(bytes),
            mode: LoadMode::Heap,
            original_file_len: metadata.len(),
        })
    }
}

fn build_open_line_index(
    source: SourceBytes,
    body_start: usize,
    body_len: usize,
    mode: LoadMode,
) -> (LineIndex, Option<PendingLineIndex>) {
    let body = &source.as_slice()[body_start..body_start + body_len];
    if mode != LoadMode::MemoryMapped || body_len <= INITIAL_MMAP_LINE_INDEX_BYTES {
        return (LineIndex::build(body), None);
    }

    let initial = LineIndex::build_prefix(body, INITIAL_MMAP_LINE_INDEX_BYTES);
    let pending = spawn_full_line_index(source, body_start, body_len);
    (initial, pending)
}

fn spawn_full_line_index(
    source: SourceBytes,
    body_start: usize,
    body_len: usize,
) -> Option<PendingLineIndex> {
    let pending = Arc::new(Mutex::new(None));
    let worker_pending = Arc::clone(&pending);
    let spawn_result = thread::Builder::new()
        .name("blitz-line-index".to_owned())
        .spawn(move || {
            let body = &source.as_slice()[body_start..body_start + body_len];
            let mut line_index = LineIndex::build_prefix(body, INITIAL_MMAP_LINE_INDEX_BYTES);
            while !line_index.is_complete() {
                line_index.extend(body, INITIAL_MMAP_LINE_INDEX_BYTES);
                thread::yield_now();
            }
            if let Ok(mut completed) = worker_pending.lock() {
                *completed = Some(line_index);
            }
        });

    match spawn_result {
        Ok(_handle) => Some(pending),
        Err(_error) => None,
    }
}

fn temporary_save_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("untitled");
    path.with_file_name(format!(".{file_name}.blitzpad-tmp"))
}

fn write_normalized_utf8_chunk<W: Write>(
    writer: &mut W,
    chunk: &[u8],
    line_ending: LineEnding,
    pending_cr: &mut bool,
    written: &mut u64,
) -> std::io::Result<()> {
    let sequence = line_ending.sequence().as_bytes();
    let mut start = 0usize;
    let mut index = 0usize;

    if *pending_cr {
        if chunk.first() == Some(&b'\n') {
            writer.write_all(sequence)?;
            *written += sequence.len() as u64;
            start = 1;
            index = 1;
        } else {
            writer.write_all(sequence)?;
            *written += sequence.len() as u64;
        }
        *pending_cr = false;
    }

    while index < chunk.len() {
        match chunk[index] {
            b'\r' => {
                write_counted(writer, &chunk[start..index], written)?;
                if chunk.get(index + 1) == Some(&b'\n') {
                    writer.write_all(sequence)?;
                    *written += sequence.len() as u64;
                    index += 2;
                    start = index;
                } else if index + 1 == chunk.len() {
                    *pending_cr = true;
                    index += 1;
                    start = index;
                } else {
                    writer.write_all(sequence)?;
                    *written += sequence.len() as u64;
                    index += 1;
                    start = index;
                }
            }
            b'\n' => {
                write_counted(writer, &chunk[start..index], written)?;
                writer.write_all(sequence)?;
                *written += sequence.len() as u64;
                index += 1;
                start = index;
            }
            _ => index += 1,
        }
    }

    write_counted(writer, &chunk[start..], written)
}

fn write_counted<W: Write>(writer: &mut W, bytes: &[u8], written: &mut u64) -> std::io::Result<()> {
    if !bytes.is_empty() {
        writer.write_all(bytes)?;
        *written += bytes.len() as u64;
    }
    Ok(())
}

fn visual_column(text_before_caret: &str) -> usize {
    text_before_caret.chars().fold(1usize, |column, character| {
        if character == '\t' {
            column + (TAB_WIDTH - ((column - 1) % TAB_WIDTH))
        } else {
            column + 1
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_beyond_partial_line_index_extends_index_first() {
        let bytes = b"a\nb\nc\nd".to_vec();
        let len = bytes.len();
        let source = SourceBytes::from_vec(bytes.clone());
        let buffer = PieceTable::from_source(source, 0, len).expect("piece table");
        let mut document = Document {
            path: None,
            buffer,
            line_index: RefCell::new(LineIndex::build_prefix(&bytes, 2)),
            pending_line_index: RefCell::new(None),
            encoding: TextEncoding::Utf8,
            line_ending: LineEnding::Lf,
            dirty: false,
            load_mode: LoadMode::Heap,
            original_file_len: len as u64,
            change_generation: 0,
            saved_generation: Some(0),
            undo_stack: Vec::new(),
        };

        document.insert_text(len, "\ne").expect("insert");

        assert_eq!(document.line_count(), 5);
        assert_eq!(document.visible_lines(4, 1)[0].text, "e");
    }

    #[test]
    fn incremental_line_index_preserves_crlf_across_stream_chunk_boundary() {
        let chunk_len = 1024 * 1024;
        let mut bytes = vec![b'a'; chunk_len - 1];
        bytes.extend_from_slice(b"\r\nb");
        let len = bytes.len();
        let source = SourceBytes::from_vec(bytes.clone());
        let buffer = PieceTable::from_source(source, 0, len).expect("piece table");
        let document = Document {
            path: None,
            buffer,
            line_index: RefCell::new(LineIndex::build_prefix(&bytes, 0)),
            pending_line_index: RefCell::new(None),
            encoding: TextEncoding::Utf8,
            line_ending: LineEnding::CrLf,
            dirty: false,
            load_mode: LoadMode::Heap,
            original_file_len: len as u64,
            change_generation: 0,
            saved_generation: Some(0),
            undo_stack: Vec::new(),
        };

        assert!(document
            .extend_line_index_towards_offset(len, len)
            .expect("extend"));

        assert_eq!(document.line_start(1), Some(chunk_len + 1));
        assert_eq!(document.visible_lines(1, 1)[0].text, "b");
    }
}
