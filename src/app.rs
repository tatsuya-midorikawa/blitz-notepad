use std::ops::Range;
use std::path::{Path, PathBuf};

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
        let Some(range) = find_range(
            &self.document.text_lossy(),
            query,
            self.caret_offset,
            match_case,
            forward,
        ) else {
            return Ok(false);
        };
        self.set_selection(range)?;
        Ok(true)
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

        let ranges = find_all_ranges(&self.document.text_lossy(), query, match_case);
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

fn find_range(
    text: &str,
    query: &str,
    caret_offset: usize,
    match_case: bool,
    forward: bool,
) -> Option<Range<usize>> {
    if query.is_empty() {
        return None;
    }

    let ranges = find_all_ranges(text, query, match_case);
    if forward {
        ranges
            .iter()
            .find(|range| range.start >= caret_offset)
            .or_else(|| ranges.first())
            .cloned()
    } else {
        ranges
            .iter()
            .rev()
            .find(|range| range.start < caret_offset)
            .or_else(|| ranges.last())
            .cloned()
    }
}

fn find_all_ranges(text: &str, query: &str, match_case: bool) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    if match_case || !text.is_ascii() || !query.is_ascii() {
        return text
            .match_indices(query)
            .map(|(start, value)| start..start + value.len())
            .collect();
    }

    let haystack = text.to_ascii_lowercase();
    let needle = query.to_ascii_lowercase();
    haystack
        .match_indices(&needle)
        .map(|(start, value)| start..start + value.len())
        .collect()
}

fn text_matches(text: &str, query: &str, match_case: bool) -> bool {
    if match_case || !text.is_ascii() || !query.is_ascii() {
        text == query
    } else {
        text.eq_ignore_ascii_case(query)
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
