use std::cell::Cell;
use std::ops::Range;
use std::path::{Path, PathBuf};

use memchr::memmem::Finder;
use memchr::{memchr, memchr2};
use time::format_description::FormatItem;
use time::macros::format_description;
use time::OffsetDateTime;

use crate::document::TextEdit;
use crate::encoding::TextEncoding;
use crate::line_index::LineEnding;
use crate::settings::EditorSettings;
use crate::ui::{default_ui_state, NotepadUiState};
use crate::{Document, Result};

const DATE_TIME_FORMAT: &[FormatItem<'_>] =
    format_description!("[hour]:[minute] [month]/[day]/[year]");

#[derive(Clone, Debug)]
pub struct BlitzApp {
    document: Document,
    settings: EditorSettings,
    caret_offset: usize,
    selection: Option<Range<usize>>,
    clipboard_has_text: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct SaveSnapshot {
    pub document: Document,
    pub path: PathBuf,
    pub encoding: TextEncoding,
    pub line_ending: LineEnding,
    pub generation: u64,
}

impl BlitzApp {
    pub fn new(settings: EditorSettings) -> Self {
        Self {
            document: Document::new_untitled(),
            settings,
            caret_offset: 0,
            selection: None,
            clipboard_has_text: false,
        }
    }

    pub fn open(path: impl AsRef<Path>, settings: EditorSettings) -> Result<Self> {
        Ok(Self {
            document: Document::open(path)?,
            settings,
            caret_offset: 0,
            selection: None,
            clipboard_has_text: false,
        })
    }

    pub fn open_document(&mut self, path: impl AsRef<Path>) -> Result<()> {
        self.document = Document::open(path)?;
        self.caret_offset = 0;
        self.selection = None;
        Ok(())
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    pub fn document_mut(&mut self) -> &mut Document {
        &mut self.document
    }

    pub fn settings(&self) -> &EditorSettings {
        &self.settings
    }

    pub fn settings_mut(&mut self) -> &mut EditorSettings {
        &mut self.settings
    }

    pub fn caret_offset(&self) -> usize {
        self.caret_offset
    }

    pub fn selection_len(&self) -> usize {
        self.selection.as_ref().map_or(0, |range| range.len())
    }

    pub fn ui_state(&self) -> Result<NotepadUiState> {
        let mut state = default_ui_state(&self.settings);
        let caret = self.document.caret_position(self.caret_offset)?;
        state.file_name = self.document.file_name();
        state.dirty = self.document.is_dirty();
        state.caret_line = caret.line;
        state.caret_column = caret.column;
        state.line_ending = self.document.line_ending();
        state.encoding = self.document.encoding();
        state.selection_bytes = self.selection.as_ref().map_or(0, |range| range.len());
        state.can_delete_forward = self.caret_offset < self.document.len();
        state.undo_available = self.document.can_undo();
        state.clipboard_has_text = self.clipboard_has_text;
        Ok(state)
    }

    pub fn set_caret_offset(&mut self, byte_offset: usize) -> Result<()> {
        self.document.caret_position(byte_offset)?;
        self.caret_offset = byte_offset;
        self.selection = None;
        Ok(())
    }

    pub fn set_selection(&mut self, range: Range<usize>) -> Result<()> {
        self.document.caret_position(range.start)?;
        self.document.caret_position(range.end)?;
        self.caret_offset = range.end;
        self.selection = Some(range);
        Ok(())
    }

    pub fn set_selection_range(&mut self, anchor: usize, focus: usize) -> Result<()> {
        self.document.caret_position(anchor)?;
        self.document.caret_position(focus)?;
        self.caret_offset = focus;
        self.selection = normalized_selection(anchor, focus);
        Ok(())
    }

    pub fn selected_range(&self) -> Option<Range<usize>> {
        self.selection.clone()
    }

    pub fn set_clipboard_has_text(&mut self, clipboard_has_text: bool) {
        self.clipboard_has_text = clipboard_has_text;
    }

    pub fn new_document(&mut self) {
        self.document = Document::new_untitled();
        self.caret_offset = 0;
        self.selection = None;
        self.clipboard_has_text = false;
    }

    pub fn select_all(&mut self) {
        self.selection = Some(0..self.document.len());
        self.caret_offset = self.document.len();
    }

    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection.as_ref()?;
        self.document.text_range_lossy(selection.clone()).ok()
    }

    pub fn cut_selection(&mut self) -> Result<Option<String>> {
        let Some(selection) = self.selection.clone() else {
            return Ok(None);
        };
        let selected = self.selected_text();
        self.document.delete_range(selection.clone())?;
        self.caret_offset = selection.start;
        self.selection = None;
        Ok(selected)
    }

    pub fn paste_text(&mut self, text: &str) -> Result<()> {
        self.insert_text(text)
    }

    pub fn find_text(&mut self, query: &str, match_case: bool, forward: bool) -> Result<bool> {
        self.find_text_with_options(query, match_case, forward, true)
    }

    pub fn find_text_with_options(
        &mut self,
        query: &str,
        match_case: bool,
        forward: bool,
        wrap_around: bool,
    ) -> Result<bool> {
        let search_offset = if forward {
            self.caret_offset
        } else {
            self.selection
                .as_ref()
                .map_or(self.caret_offset, |range| range.start)
        };

        if let Some(range) =
            self.fast_find_range(query, match_case, forward, wrap_around, search_offset)?
        {
            self.set_selection(range)?;
            return Ok(true);
        }

        Ok(false)
    }

    fn fast_find_range(
        &self,
        query: &str,
        match_case: bool,
        forward: bool,
        wrap_around: bool,
        search_offset: usize,
    ) -> Result<Option<Range<usize>>> {
        if query.is_empty() {
            return Ok(None);
        }

        if forward {
            let Some(range) =
                find_streaming_forward(self.document(), query, search_offset, match_case)?
            else {
                if wrap_around {
                    return find_streaming_forward(self.document(), query, 0, match_case);
                }
                return Ok(None);
            };
            Ok(Some(range))
        } else {
            let Some(range) =
                find_streaming_backward(self.document(), query, search_offset, match_case)?
            else {
                if wrap_around {
                    return find_streaming_backward(
                        self.document(),
                        query,
                        self.document.len(),
                        match_case,
                    );
                }
                return Ok(None);
            };
            Ok(Some(range))
        }
    }

    pub fn replace_next(
        &mut self,
        query: &str,
        replacement: &str,
        match_case: bool,
    ) -> Result<bool> {
        if let Some(selection) = self.selection.clone() {
            let selected = self.selected_text().unwrap_or_default();
            if text_matches(&selected, query, match_case) {
                let start = selection.start;
                self.document.replace_range(selection, replacement)?;
                self.caret_offset = start + replacement.len();
                self.selection = None;
                return Ok(true);
            }
        }

        if self.find_text(query, match_case, true)? {
            self.replace_next(query, replacement, match_case)
        } else {
            Ok(false)
        }
    }

    pub fn replace_all(
        &mut self,
        query: &str,
        replacement: &str,
        match_case: bool,
    ) -> Result<usize> {
        if query.is_empty() {
            return Ok(0);
        }

        let ranges = find_all_document_ranges(&self.document, query, match_case);
        let edits = ranges
            .iter()
            .map(|range| TextEdit {
                range: range.clone(),
                replacement: replacement.to_owned(),
            })
            .collect::<Vec<_>>();
        self.document.apply_edits_from_end(&edits)?;
        self.caret_offset = self.document.len().min(self.caret_offset);
        self.selection = None;
        Ok(ranges.len())
    }

    pub fn go_to_line(&mut self, one_based_line: usize) -> Result<bool> {
        if one_based_line == 0 {
            return Ok(false);
        }
        let Some(offset) = self.document.line_start(one_based_line - 1) else {
            return Ok(false);
        };
        self.set_caret_offset(offset)?;
        Ok(true)
    }

    pub fn insert_text(&mut self, text: &str) -> Result<()> {
        if let Some(selection) = self.selection.take() {
            let start = selection.start;
            self.document.replace_range(selection, text)?;
            self.caret_offset = start + text.len();
        } else {
            self.document.insert_text(self.caret_offset, text)?;
            self.caret_offset += text.len();
        }
        Ok(())
    }

    pub fn delete_selection(&mut self) -> Result<bool> {
        let Some(selection) = self.selection.take() else {
            return Ok(false);
        };
        let start = selection.start;
        self.document.delete_range(selection)?;
        self.caret_offset = start;
        Ok(true)
    }

    pub fn delete_forward(&mut self) -> Result<bool> {
        if self.delete_selection()? {
            return Ok(true);
        }

        let Some(next_offset) = self.document.next_char_offset(self.caret_offset)? else {
            return Ok(false);
        };
        self.document.delete_range(self.caret_offset..next_offset)?;
        Ok(true)
    }

    pub fn backspace(&mut self) -> Result<bool> {
        if self.delete_selection()? {
            return Ok(true);
        }

        let Some(previous_offset) = self.document.previous_char_offset(self.caret_offset)? else {
            return Ok(false);
        };

        self.document
            .delete_range(previous_offset..self.caret_offset)?;
        self.caret_offset = previous_offset;
        Ok(true)
    }

    pub fn move_left(&mut self) -> Result<()> {
        if let Some(previous_offset) = self.document.previous_char_offset(self.caret_offset)? {
            self.set_caret_offset(previous_offset)?;
        }
        Ok(())
    }

    pub fn move_right(&mut self) -> Result<()> {
        if let Some(next_offset) = self.document.next_char_offset(self.caret_offset)? {
            self.set_caret_offset(next_offset)?;
        }
        Ok(())
    }

    pub fn move_line_start(&mut self) -> Result<()> {
        let range = self
            .document
            .line_content_range_for_offset(self.caret_offset)?;
        self.set_caret_offset(range.start)
    }

    pub fn move_line_end(&mut self) -> Result<()> {
        let range = self
            .document
            .line_content_range_for_offset(self.caret_offset)?;
        self.set_caret_offset(range.end)
    }

    pub fn move_up(&mut self) -> Result<()> {
        self.move_vertical(-1)
    }

    pub fn move_down(&mut self) -> Result<()> {
        self.move_vertical(1)
    }

    pub fn undo(&mut self) -> Result<bool> {
        let undone = self.document.undo()?;
        self.caret_offset = self.caret_offset.min(self.document.len());
        self.selection = None;
        Ok(undone)
    }

    pub fn save(&mut self) -> Result<bool> {
        if self.document.path().is_none() {
            return Ok(false);
        }
        self.document.save()?;
        Ok(true)
    }

    pub(crate) fn save_snapshot(&self) -> Option<SaveSnapshot> {
        Some(SaveSnapshot {
            document: self.document.clone(),
            path: self.document.path()?.to_path_buf(),
            encoding: self.document.encoding(),
            line_ending: self.document.line_ending(),
            generation: self.document.change_generation(),
        })
    }

    pub(crate) fn complete_save_snapshot(&mut self, generation: u64, saved_len: u64) -> bool {
        self.document.mark_saved_generation(generation, saved_len)
    }

    pub fn save_as_path(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let encoding = self.document.encoding();
        let line_ending = self.document.line_ending();
        self.document.save_as(path, encoding, line_ending)
    }

    fn move_vertical(&mut self, direction: isize) -> Result<()> {
        let line_index = self.document.line_for_offset(self.caret_offset)?;
        let target_line = match direction {
            -1 if line_index > 0 => line_index - 1,
            1 if line_index + 1 < self.document.line_count() => line_index + 1,
            _ => return Ok(()),
        };

        let preferred_column = self.document.char_column_for_offset(self.caret_offset)?;
        let offset = self
            .document
            .offset_for_char_column(target_line, preferred_column)?;
        self.set_caret_offset(offset)
    }

    pub fn apply_multi_cursor_text(
        &mut self,
        ranges: Vec<Range<usize>>,
        replacement: &str,
    ) -> Result<()> {
        let edits = ranges
            .into_iter()
            .map(|range| TextEdit {
                range,
                replacement: replacement.to_owned(),
            })
            .collect::<Vec<_>>();
        self.document.apply_edits_from_end(&edits)
    }

    pub fn insert_time_date(&mut self) -> Result<()> {
        let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
        let formatted = now
            .format(DATE_TIME_FORMAT)
            .unwrap_or_else(|_| "00:00 1/1/1970".to_owned());
        self.insert_text(&formatted)
    }

    pub fn zoom_in(&mut self) {
        let next = next_zoom(self.settings.zoom_percent, ZoomDirection::In);
        self.settings.set_zoom_percent(next);
    }

    pub fn zoom_out(&mut self) {
        let next = next_zoom(self.settings.zoom_percent, ZoomDirection::Out);
        self.settings.set_zoom_percent(next);
    }

    pub fn restore_default_zoom(&mut self) {
        self.settings.set_zoom_percent(100);
    }

    pub fn toggle_word_wrap(&mut self) {
        self.settings.word_wrap = !self.settings.word_wrap;
    }

    pub fn toggle_status_bar(&mut self) {
        self.settings.status_bar_visible = !self.settings.status_bar_visible;
    }
}

fn find_streaming_forward(
    document: &Document,
    query: &str,
    start_offset: usize,
    match_case: bool,
) -> Result<Option<Range<usize>>> {
    let needle = search_needle(query, match_case);
    if needle.is_empty() {
        return Ok(None);
    }
    let finder = Finder::new(&needle);

    let found_start = Cell::new(None::<usize>);
    let absolute_offset = Cell::new(0usize);
    let mut tail = Vec::new();
    let overlap = needle.len().saturating_sub(1);

    let _ = document.for_each_chunk(|chunk| {
        if found_start.get().is_some() {
            return Err(());
        }

        let chunk_start = absolute_offset.get();
        let boundary_match = find_boundary_match(
            &tail,
            chunk,
            chunk_start,
            start_offset,
            &needle,
            &finder,
            match_case,
        );
        let chunk_match = find_bytes_from(
            chunk,
            &needle,
            &finder,
            match_case,
            start_offset.saturating_sub(chunk_start),
        )
        .map(|relative_start| chunk_start + relative_start);

        if let Some(start) = match (boundary_match, chunk_match) {
            (Some(boundary), Some(chunk)) => Some(boundary.min(chunk)),
            (Some(boundary), None) => Some(boundary),
            (None, Some(chunk)) => Some(chunk),
            (None, None) => None,
        } {
            found_start.set(Some(start));
            return Err(());
        }

        update_tail(&mut tail, chunk, overlap);
        absolute_offset.set(chunk_start + chunk.len());
        Ok(())
    });

    Ok(found_start
        .into_inner()
        .map(|start| start..start + needle.len()))
}

fn find_streaming_backward(
    document: &Document,
    query: &str,
    before_offset: usize,
    match_case: bool,
) -> Result<Option<Range<usize>>> {
    if before_offset == 0 {
        return Ok(None);
    }

    let needle = search_needle(query, match_case);
    if needle.is_empty() {
        return Ok(None);
    }

    if before_offset <= document.len() / 2 {
        find_streaming_backward_from_start(document, query, before_offset, match_case)
    } else {
        find_streaming_backward_from_end(document, query, before_offset, match_case)
    }
}

fn find_streaming_backward_from_start(
    document: &Document,
    query: &str,
    before_offset: usize,
    match_case: bool,
) -> Result<Option<Range<usize>>> {
    let needle = search_needle(query, match_case);
    if needle.is_empty() {
        return Ok(None);
    }
    let finder = Finder::new(&needle);

    let last_start = Cell::new(None::<usize>);
    let absolute_offset = Cell::new(0usize);
    let mut tail = Vec::new();
    let overlap = needle.len().saturating_sub(1);

    let _ = document.for_each_chunk(|chunk| {
        let chunk_start = absolute_offset.get();
        if chunk_start.saturating_sub(tail.len()) >= before_offset {
            return Err(());
        }

        visit_boundary_matches(
            &tail,
            chunk,
            chunk_start,
            &needle,
            &finder,
            match_case,
            |start| {
                if start < before_offset {
                    last_start.set(Some(start));
                }
            },
        );
        find_all_bytes_overlapping(chunk, &needle, &finder, match_case, |relative_start| {
            let start = chunk_start + relative_start;
            if start < before_offset {
                last_start.set(Some(start));
            }
        });

        update_tail(&mut tail, chunk, overlap);
        absolute_offset.set(chunk_start + chunk.len());
        Ok(())
    });

    Ok(last_start
        .into_inner()
        .map(|start| start..start + needle.len()))
}

fn find_streaming_backward_from_end(
    document: &Document,
    query: &str,
    before_offset: usize,
    match_case: bool,
) -> Result<Option<Range<usize>>> {
    let needle = search_needle(query, match_case);
    if needle.is_empty() {
        return Ok(None);
    }
    let finder = Finder::new(&needle);

    let found_start = Cell::new(None::<usize>);
    let mut head = Vec::new();
    let overlap = needle.len().saturating_sub(1);

    let _ = document.for_each_chunk_rev(|chunk_start, chunk| {
        let boundary_match = find_last_boundary_match_before(
            &head,
            chunk,
            chunk_start,
            before_offset,
            &needle,
            &finder,
            match_case,
        );
        let chunk_match = find_last_bytes_before(
            chunk,
            &needle,
            &finder,
            match_case,
            before_offset.saturating_sub(chunk_start),
        )
        .map(|relative_start| chunk_start + relative_start);

        if let Some(start) = match (boundary_match, chunk_match) {
            (Some(boundary), Some(chunk)) => Some(boundary.max(chunk)),
            (Some(boundary), None) => Some(boundary),
            (None, Some(chunk)) => Some(chunk),
            (None, None) => None,
        } {
            found_start.set(Some(start));
            return Err(());
        }

        update_head(&mut head, chunk, overlap);
        Ok(())
    });

    Ok(found_start
        .into_inner()
        .map(|start| start..start + needle.len()))
}

fn find_all_document_ranges(
    document: &Document,
    query: &str,
    match_case: bool,
) -> Vec<Range<usize>> {
    let needle = search_needle(query, match_case);
    if needle.is_empty() {
        return Vec::new();
    }
    let finder = Finder::new(&needle);
    let mut ranges = Vec::new();
    let next_allowed_start = Cell::new(0usize);
    let absolute_offset = Cell::new(0usize);
    let mut tail = Vec::new();
    let overlap = needle.len().saturating_sub(1);

    let _ = document.for_each_chunk(|chunk| {
        let chunk_start = absolute_offset.get();
        let mut searchable = Vec::with_capacity(tail.len() + chunk.len());
        searchable.extend_from_slice(&tail);
        searchable.extend_from_slice(chunk);
        let searchable_start = chunk_start.saturating_sub(tail.len());

        visit_global_non_overlapping_matches(
            &searchable,
            searchable_start,
            chunk_start,
            &needle,
            &finder,
            match_case,
            &next_allowed_start,
            |start| ranges.push(start..start + needle.len()),
        );

        update_tail(&mut tail, chunk, overlap);
        absolute_offset.set(chunk_start + chunk.len());
        Ok::<(), ()>(())
    });

    ranges
}

fn search_needle(query: &str, match_case: bool) -> Vec<u8> {
    if match_case {
        query.as_bytes().to_vec()
    } else {
        query
            .bytes()
            .map(|byte| byte.to_ascii_lowercase())
            .collect()
    }
}

fn visit_global_non_overlapping_matches(
    bytes: &[u8],
    bytes_start: usize,
    fresh_start: usize,
    needle: &[u8],
    finder: &Finder<'_>,
    match_case: bool,
    next_allowed_start: &Cell<usize>,
    mut visit: impl FnMut(usize),
) {
    find_all_bytes(bytes, needle, finder, match_case, |relative_start| {
        let start = bytes_start + relative_start;
        let end = start + needle.len();
        if start < fresh_start && end <= fresh_start {
            return;
        }
        if start < next_allowed_start.get() {
            return;
        }

        visit(start);
        next_allowed_start.set(end);
    });
}

fn find_bytes_from(
    bytes: &[u8],
    needle: &[u8],
    finder: &Finder<'_>,
    match_case: bool,
    start_at: usize,
) -> Option<usize> {
    let needle_len = needle.len();
    if needle_len > bytes.len() {
        return None;
    }

    let max_start = bytes.len() - needle_len;
    if start_at > max_start {
        return None;
    }

    if match_case {
        finder
            .find(&bytes[start_at..])
            .map(|relative| start_at + relative)
    } else {
        find_ascii_case_insensitive_from(bytes, needle, start_at)
    }
}

fn find_all_bytes(
    bytes: &[u8],
    needle: &[u8],
    finder: &Finder<'_>,
    match_case: bool,
    mut visit: impl FnMut(usize),
) {
    let needle_len = needle.len();
    if needle_len > bytes.len() {
        return;
    }

    if match_case {
        let mut offset = 0;
        while let Some(relative) = finder.find(&bytes[offset..]) {
            let index = offset + relative;
            visit(index);
            offset = index + needle_len;
            if offset > bytes.len().saturating_sub(needle_len) {
                break;
            }
        }
    } else {
        let mut offset = 0;
        while let Some(index) = find_ascii_case_insensitive_from(bytes, needle, offset) {
            visit(index);
            offset = index + needle_len;
            if offset > bytes.len().saturating_sub(needle_len) {
                break;
            }
        }
    }
}

fn find_all_bytes_overlapping(
    bytes: &[u8],
    needle: &[u8],
    finder: &Finder<'_>,
    match_case: bool,
    mut visit: impl FnMut(usize),
) {
    let needle_len = needle.len();
    if needle_len > bytes.len() {
        return;
    }

    let max_start = bytes.len() - needle_len;
    if match_case {
        let mut offset = 0;
        while let Some(relative) = finder.find(&bytes[offset..]) {
            let index = offset + relative;
            if index > max_start {
                break;
            }
            visit(index);
            offset = index + 1;
            if offset > max_start {
                break;
            }
        }
    } else {
        let mut offset = 0;
        while let Some(index) = find_ascii_case_insensitive_from(bytes, needle, offset) {
            if index > max_start {
                break;
            }
            visit(index);
            offset = index + 1;
            if offset > max_start {
                break;
            }
        }
    }
}

fn find_last_bytes_before(
    bytes: &[u8],
    needle: &[u8],
    finder: &Finder<'_>,
    match_case: bool,
    before_relative: usize,
) -> Option<usize> {
    if before_relative == 0 {
        return None;
    }

    let mut last_start = None;
    find_all_bytes_overlapping(bytes, needle, finder, match_case, |relative_start| {
        if relative_start < before_relative {
            last_start = Some(relative_start);
        }
    });
    last_start
}

fn find_boundary_match(
    tail: &[u8],
    chunk: &[u8],
    chunk_start: usize,
    start_offset: usize,
    needle: &[u8],
    finder: &Finder<'_>,
    match_case: bool,
) -> Option<usize> {
    if tail.is_empty() || needle.len() <= 1 {
        return None;
    }

    let prefix_len = needle.len().saturating_sub(1).min(chunk.len());
    let mut boundary = Vec::with_capacity(tail.len() + prefix_len);
    boundary.extend_from_slice(tail);
    boundary.extend_from_slice(&chunk[..prefix_len]);
    let boundary_start = chunk_start.saturating_sub(tail.len());
    let relative_start = find_bytes_from(
        &boundary,
        needle,
        finder,
        match_case,
        start_offset.saturating_sub(boundary_start),
    )?;
    if relative_start < tail.len() && relative_start + needle.len() > tail.len() {
        Some(boundary_start + relative_start)
    } else {
        None
    }
}

fn visit_boundary_matches(
    tail: &[u8],
    chunk: &[u8],
    chunk_start: usize,
    needle: &[u8],
    finder: &Finder<'_>,
    match_case: bool,
    mut visit: impl FnMut(usize),
) {
    if tail.is_empty() || needle.len() <= 1 {
        return;
    }

    let prefix_len = needle.len().saturating_sub(1).min(chunk.len());
    let mut boundary = Vec::with_capacity(tail.len() + prefix_len);
    boundary.extend_from_slice(tail);
    boundary.extend_from_slice(&chunk[..prefix_len]);
    let boundary_start = chunk_start.saturating_sub(tail.len());
    find_all_bytes_overlapping(&boundary, needle, finder, match_case, |relative_start| {
        if relative_start < tail.len() && relative_start + needle.len() > tail.len() {
            visit(boundary_start + relative_start);
        }
    });
}

fn find_last_boundary_match_before(
    head: &[u8],
    chunk: &[u8],
    chunk_start: usize,
    before_offset: usize,
    needle: &[u8],
    finder: &Finder<'_>,
    match_case: bool,
) -> Option<usize> {
    if head.is_empty() || needle.len() <= 1 {
        return None;
    }

    let suffix_len = needle.len().saturating_sub(1).min(chunk.len());
    let mut boundary = Vec::with_capacity(suffix_len + head.len());
    boundary.extend_from_slice(&chunk[chunk.len() - suffix_len..]);
    boundary.extend_from_slice(head);
    let boundary_start = chunk_start + chunk.len() - suffix_len;
    let mut last_start = None;
    find_all_bytes_overlapping(&boundary, needle, finder, match_case, |relative_start| {
        let start = boundary_start + relative_start;
        if relative_start < suffix_len
            && relative_start + needle.len() > suffix_len
            && start < before_offset
        {
            last_start = Some(start);
        }
    });
    last_start
}

fn update_tail(tail: &mut Vec<u8>, chunk: &[u8], overlap: usize) {
    if overlap == 0 {
        tail.clear();
        return;
    }

    if chunk.len() >= overlap {
        tail.clear();
        tail.extend_from_slice(&chunk[chunk.len() - overlap..]);
    } else {
        tail.extend_from_slice(chunk);
        let extra = tail.len().saturating_sub(overlap);
        if extra > 0 {
            tail.drain(..extra);
        }
    }
}

fn update_head(head: &mut Vec<u8>, chunk: &[u8], overlap: usize) {
    if overlap == 0 {
        head.clear();
        return;
    }

    if chunk.len() >= overlap {
        head.clear();
        head.extend_from_slice(&chunk[..overlap]);
    } else {
        let keep_from_head = (overlap - chunk.len()).min(head.len());
        let mut next = Vec::with_capacity(chunk.len() + keep_from_head);
        next.extend_from_slice(chunk);
        next.extend_from_slice(&head[..keep_from_head]);
        *head = next;
    }
}

fn find_ascii_case_insensitive_from(bytes: &[u8], needle: &[u8], start_at: usize) -> Option<usize> {
    let max_start = bytes.len().checked_sub(needle.len())?;
    let first = needle[0];
    let first_upper = first.to_ascii_uppercase();
    let mut offset = start_at;

    while offset <= max_start {
        let relative = if first == first_upper {
            memchr(first, &bytes[offset..])?
        } else {
            memchr2(first, first_upper, &bytes[offset..])?
        };
        let index = offset + relative;
        if index > max_start {
            return None;
        }
        if bytes[index..index + needle.len()]
            .iter()
            .zip(needle)
            .all(|(haystack, needle)| haystack.to_ascii_lowercase() == *needle)
        {
            return Some(index);
        }
        offset = index + 1;
    }

    None
}

fn text_matches(text: &str, query: &str, match_case: bool) -> bool {
    if match_case {
        text == query
    } else {
        search_needle(text, false) == search_needle(query, false)
    }
}

fn normalized_selection(anchor: usize, focus: usize) -> Option<Range<usize>> {
    match anchor.cmp(&focus) {
        std::cmp::Ordering::Less => Some(anchor..focus),
        std::cmp::Ordering::Greater => Some(focus..anchor),
        std::cmp::Ordering::Equal => None,
    }
}

impl Default for BlitzApp {
    fn default() -> Self {
        Self::new(EditorSettings::default())
    }
}

#[derive(Clone, Copy)]
enum ZoomDirection {
    In,
    Out,
}

fn next_zoom(current: u16, direction: ZoomDirection) -> u16 {
    const STEPS: [u16; 18] = [
        10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 125, 150, 175, 200, 300, 400, 450, 500,
    ];
    match direction {
        ZoomDirection::In => STEPS
            .iter()
            .copied()
            .find(|step| *step > current)
            .unwrap_or(500),
        ZoomDirection::Out => STEPS
            .iter()
            .rev()
            .copied()
            .find(|step| *step < current)
            .unwrap_or(10),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_steps_match_notepad_style_bounds() {
        let mut app = BlitzApp::default();
        app.zoom_in();
        assert_eq!(app.settings.zoom_percent, 125);
        app.restore_default_zoom();
        app.zoom_out();
        assert_eq!(app.settings.zoom_percent, 90);
    }

    #[test]
    fn keyboard_editing_operations_update_text_and_caret() {
        let mut app = BlitzApp::default();
        app.insert_text("abc\ndef").expect("insert");
        app.set_caret_offset(2).expect("caret");
        app.insert_text("X").expect("insert");
        assert_eq!(app.document().text_lossy(), "abXc\ndef");

        app.backspace().expect("backspace");
        assert_eq!(app.document().text_lossy(), "abc\ndef");
        assert_eq!(app.caret_offset(), 2);

        app.move_down().expect("down");
        assert_eq!(
            app.document()
                .caret_position(app.caret_offset())
                .unwrap()
                .line,
            2
        );
        app.move_line_end().expect("end");
        app.delete_forward().expect("delete at end");
        assert_eq!(app.document().text_lossy(), "abc\ndef");
    }

    #[test]
    fn select_all_replaces_document_text() {
        let mut app = BlitzApp::default();
        app.insert_text("alpha").expect("insert");
        app.select_all();
        app.insert_text("beta").expect("replace");

        assert_eq!(app.document().text_lossy(), "beta");
        assert_eq!(app.caret_offset(), "beta".len());
    }

    #[test]
    fn selection_range_normalizes_anchor_and_focus() {
        let mut app = BlitzApp::default();
        app.insert_text("abcdef").expect("insert");

        app.set_selection_range(5, 2).expect("selection");

        assert_eq!(app.selected_range(), Some(2..5));
        assert_eq!(app.caret_offset(), 2);
        assert_eq!(app.selected_text().as_deref(), Some("cde"));
    }

    #[test]
    fn find_replace_and_go_to_work() {
        let mut app = BlitzApp::default();
        app.insert_text("alpha\nbeta\nALPHA").expect("insert");
        app.set_caret_offset(0).expect("caret");

        assert!(app.find_text("ALPHA", false, true).expect("find"));
        assert_eq!(app.selected_range(), Some(0..5));

        assert!(app.replace_next("alpha", "one", false).expect("replace"));
        assert_eq!(app.document().text_lossy(), "one\nbeta\nALPHA");

        assert_eq!(
            app.replace_all("beta", "two", false).expect("replace all"),
            1
        );
        assert_eq!(app.document().text_lossy(), "one\ntwo\nALPHA");

        assert!(app.go_to_line(2).expect("go to"));
        assert_eq!(
            app.document()
                .caret_position(app.caret_offset())
                .unwrap()
                .line,
            2
        );
    }

    #[test]
    fn find_text_can_disable_wrap_around() {
        let mut app = BlitzApp::default();
        app.insert_text("one two one").expect("insert");
        app.set_caret_offset("one two one".len()).expect("caret");

        assert!(!app
            .find_text_with_options("one", true, true, false)
            .expect("find without wrap"));
        assert!(app
            .find_text_with_options("one", true, true, true)
            .expect("find with wrap"));
        assert_eq!(app.selected_range(), Some(0..3));
    }

    #[test]
    fn fast_find_starts_at_caret_within_same_chunk() {
        let mut app = BlitzApp::default();
        app.insert_text("one one one").expect("insert");
        app.set_caret_offset(4).expect("caret");

        assert!(app
            .find_text_with_options("one", true, true, false)
            .expect("find"));

        assert_eq!(app.selected_range(), Some(4..7));
    }

    #[test]
    fn fast_find_forward_can_start_inside_self_overlapping_match() {
        let mut app = BlitzApp::default();
        app.insert_text("aaa").expect("insert");
        app.set_caret_offset(1).expect("caret");

        assert!(app
            .find_text_with_options("aa", true, true, false)
            .expect("find"));

        assert_eq!(app.selected_range(), Some(1..3));
    }

    #[test]
    fn fast_find_is_ascii_case_insensitive_when_requested() {
        let mut app = BlitzApp::default();
        app.insert_text("alpha BETA").expect("insert");
        app.set_caret_offset(0).expect("caret");

        assert!(app
            .find_text_with_options("beta", false, true, false)
            .expect("find"));

        assert_eq!(app.selected_range(), Some(6..10));
    }

    #[test]
    fn fast_find_backward_finds_previous_ascii_match() {
        let mut app = BlitzApp::default();
        app.insert_text("one two one").expect("insert");
        app.set_caret_offset("one two one".len()).expect("caret");

        assert!(app
            .find_text_with_options("one", true, false, false)
            .expect("find backward"));

        assert_eq!(app.selected_range(), Some(8..11));
    }

    #[test]
    fn fast_find_backward_repeated_search_skips_current_selection() {
        let mut app = BlitzApp::default();
        app.insert_text("one two one").expect("insert");
        app.set_caret_offset("one two one".len()).expect("caret");

        assert!(app
            .find_text_with_options("one", true, false, false)
            .expect("first find"));
        assert_eq!(app.selected_range(), Some(8..11));
        assert!(app
            .find_text_with_options("one", true, false, false)
            .expect("second find"));

        assert_eq!(app.selected_range(), Some(0..3));
    }

    #[test]
    fn fast_find_backward_can_wrap() {
        let mut app = BlitzApp::default();
        app.insert_text("one two one").expect("insert");
        app.set_caret_offset(0).expect("caret");

        assert!(!app
            .find_text_with_options("one", true, false, false)
            .expect("find no wrap"));
        assert!(app
            .find_text_with_options("one", true, false, true)
            .expect("find wrap"));

        assert_eq!(app.selected_range(), Some(8..11));
    }

    #[test]
    fn fast_find_backward_uses_nearest_self_overlapping_match() {
        let mut app = BlitzApp::default();
        app.insert_text("aaa").expect("insert");
        app.set_caret_offset(3).expect("caret");

        assert!(app
            .find_text_with_options("aa", true, false, false)
            .expect("find"));

        assert_eq!(app.selected_range(), Some(1..3));
    }

    #[test]
    fn fast_find_backward_handles_self_overlap_near_end_of_chunked_document() {
        let mut app = BlitzApp::default();
        let text = "a".repeat(1024 * 1024 + 3);
        app.insert_text(&text).expect("insert");
        app.set_caret_offset(text.len()).expect("caret");

        assert!(app
            .find_text_with_options("aa", true, false, false)
            .expect("find"));

        assert_eq!(app.selected_range(), Some(text.len() - 2..text.len()));
    }

    #[test]
    fn fast_find_supports_exact_non_ascii_queries() {
        let mut app = BlitzApp::default();
        app.insert_text("alpha テスト 😀 beta").expect("insert");
        app.set_caret_offset(0).expect("caret");

        assert!(app
            .find_text_with_options("テスト 😀", true, true, false)
            .expect("find"));

        assert_eq!(app.selected_text().as_deref(), Some("テスト 😀"));
    }

    #[test]
    fn fast_find_streams_non_ascii_queries_without_match_case() {
        let mut app = BlitzApp::default();
        app.insert_text("alpha テスト 😀 beta").expect("insert");
        app.set_caret_offset(0).expect("caret");

        assert!(app
            .find_text_with_options("テスト 😀", false, true, false)
            .expect("find"));

        assert_eq!(app.selected_text().as_deref(), Some("テスト 😀"));
    }

    #[test]
    fn replace_next_uses_streaming_search_case_rules_for_mixed_text() {
        let mut app = BlitzApp::default();
        app.insert_text("alpha 日本A beta").expect("insert");
        app.set_caret_offset(0).expect("caret");

        assert!(app.replace_next("日本a", "done", false).expect("replace"));

        assert_eq!(app.document().text_lossy(), "alpha done beta");
    }

    #[test]
    fn replace_all_keeps_non_overlapping_match_behavior() {
        let mut app = BlitzApp::default();
        app.insert_text("aaaa").expect("insert");

        assert_eq!(app.replace_all("aa", "b", true).expect("replace all"), 2);

        assert_eq!(app.document().text_lossy(), "bb");
    }

    #[test]
    fn fast_find_handles_match_across_piece_boundaries_after_edit() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("piece-boundary.txt");
        std::fs::write(&path, "abcXYZ").expect("write");
        let mut app = BlitzApp::open(&path, EditorSettings::default()).expect("open");
        app.set_caret_offset(3).expect("caret");
        app.insert_text("de").expect("insert");
        app.set_caret_offset(0).expect("caret");

        assert!(app
            .find_text_with_options("cdeX", true, true, false)
            .expect("find"));

        assert_eq!(app.selected_text().as_deref(), Some("cdeX"));
    }

    #[test]
    fn fast_find_backward_handles_match_across_piece_boundaries_after_edit() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("piece-boundary-backward.txt");
        std::fs::write(&path, "abcXYZ").expect("write");
        let mut app = BlitzApp::open(&path, EditorSettings::default()).expect("open");
        app.set_caret_offset(3).expect("caret");
        app.insert_text("de").expect("insert");
        app.set_caret_offset(app.document().len()).expect("caret");

        assert!(app
            .find_text_with_options("cdeX", true, false, false)
            .expect("find"));

        assert_eq!(app.selected_text().as_deref(), Some("cdeX"));
    }

    #[test]
    fn fast_find_backward_uses_nearest_self_overlapping_match_across_pieces() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("piece-overlap.txt");
        std::fs::write(&path, "aa").expect("write");
        let mut app = BlitzApp::open(&path, EditorSettings::default()).expect("open");
        app.set_caret_offset(1).expect("caret");
        app.insert_text("a").expect("insert");
        app.set_caret_offset(app.document().len()).expect("caret");

        assert!(app
            .find_text_with_options("aa", true, false, false)
            .expect("find"));

        assert_eq!(app.selected_range(), Some(1..3));
    }

    #[test]
    fn fast_find_forward_handles_match_across_stream_chunks() {
        let mut app = BlitzApp::default();
        let mut text = "a".repeat(1024 * 1024 - 2);
        text.push_str("xyZtail");
        app.insert_text(&text).expect("insert");
        app.set_caret_offset(0).expect("caret");

        assert!(app
            .find_text_with_options("xyZ", true, true, false)
            .expect("find"));

        assert_eq!(app.selected_range(), Some(1024 * 1024 - 2..1024 * 1024 + 1));
    }

    #[test]
    fn fast_find_backward_handles_match_across_stream_chunks() {
        let mut app = BlitzApp::default();
        let mut text = "a".repeat(1024 * 1024 - 2);
        text.push_str("xyZtail");
        app.insert_text(&text).expect("insert");
        app.set_caret_offset(text.len()).expect("caret");

        assert!(app
            .find_text_with_options("xyz", false, false, false)
            .expect("find"));

        assert_eq!(app.selected_range(), Some(1024 * 1024 - 2..1024 * 1024 + 1));
    }

    #[test]
    fn save_snapshot_completion_clears_dirty_only_for_same_generation() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("snapshot.txt");
        let mut app = BlitzApp::default();
        app.insert_text("alpha").expect("insert");
        app.save_as_path(&path).expect("initial save");
        app.insert_text(" beta").expect("edit");

        let snapshot = app.save_snapshot().expect("snapshot");
        let saved_len = snapshot
            .document
            .save_snapshot_to_path(&snapshot.path, snapshot.encoding, snapshot.line_ending)
            .expect("save snapshot");

        assert!(app.complete_save_snapshot(snapshot.generation, saved_len));
        assert!(!app.document().is_dirty());
        assert_eq!(std::fs::read(&path).expect("read"), b"alpha beta");

        app.insert_text(" gamma").expect("edit again");
        let stale_generation = snapshot.generation;
        assert!(!app.complete_save_snapshot(stale_generation, saved_len));
        assert!(app.document().is_dirty());
    }
}
