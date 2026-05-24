use std::fs::{self, File};
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use crate::encoding::{decode_to_utf8, detect_encoding, encode_from_utf8, TextEncoding};
use crate::error::io_path;
use crate::line_index::{detect_line_ending, normalize_line_endings, LineEnding, LineIndex};
use crate::piece_table::{PieceTable, SourceBytes};
use crate::{BlitzError, Result};

pub const MMAP_THRESHOLD_BYTES: u64 = 50 * 1024 * 1024;
const UNDO_LIMIT: usize = 100;
const TAB_WIDTH: usize = 8;

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
    bytes: Vec<u8>,
    change_generation: u64,
}

#[derive(Clone, Debug)]
pub struct Document {
    path: Option<PathBuf>,
    buffer: PieceTable,
    line_index: LineIndex,
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
            line_index: LineIndex::build(&[]),
            encoding: TextEncoding::Utf8,
            line_ending: LineEnding::CrLf,
            dirty: true,
            load_mode: LoadMode::Heap,
            original_file_len: 0,
            change_generation: 0,
            saved_generation: None,
            undo_stack: Vec::new(),
        }
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let loaded = load_source(path)?;
        let encoding = detect_encoding(loaded.source.as_slice());

        let (buffer, line_index, line_ending) = match encoding {
            TextEncoding::Utf8 | TextEncoding::Utf8Bom => {
                let body_start = encoding.bom_len(loaded.source.as_slice());
                let body_len = loaded.source.len().saturating_sub(body_start);
                let body = &loaded.source.as_slice()[body_start..body_start + body_len];
                (
                    PieceTable::from_source(loaded.source.clone(), body_start, body_len)?,
                    LineIndex::build(body),
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
                    line_ending,
                )
            }
        };

        Ok(Self {
            path: Some(path.to_path_buf()),
            buffer,
            line_index,
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

    pub fn line_count(&self) -> usize {
        self.line_index.line_count()
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

    pub fn bytes(&self) -> Vec<u8> {
        self.buffer.collect_bytes()
    }

    pub fn insert_text(&mut self, byte_offset: usize, text: &str) -> Result<()> {
        let previous = self.buffer.collect_bytes();
        self.buffer.insert_str(byte_offset, text)?;
        self.commit_successful_edit(previous);
        Ok(())
    }

    pub fn delete_range(&mut self, range: Range<usize>) -> Result<()> {
        let previous = self.buffer.collect_bytes();
        self.buffer.delete_range(range)?;
        self.commit_successful_edit(previous);
        Ok(())
    }

    pub fn replace_range(&mut self, range: Range<usize>, text: &str) -> Result<()> {
        let previous = self.buffer.collect_bytes();
        self.buffer.replace_range(range, text)?;
        self.commit_successful_edit(previous);
        Ok(())
    }

    pub fn apply_edits_from_end(&mut self, edits: &[TextEdit]) -> Result<()> {
        if edits.is_empty() {
            return Ok(());
        }

        let previous = self.buffer.collect_bytes();
        let mut next_buffer = self.buffer.clone();
        let mut sorted = edits.to_vec();
        sorted.sort_by(|left, right| right.range.start.cmp(&left.range.start));
        for edit in sorted {
            next_buffer.replace_range(edit.range, &edit.replacement)?;
        }
        self.buffer = next_buffer;
        self.commit_successful_edit(previous);
        Ok(())
    }

    pub fn undo(&mut self) -> Result<bool> {
        let Some(snapshot) = self.undo_stack.pop() else {
            return Ok(false);
        };
        let bytes = snapshot.bytes;
        let len = bytes.len();
        self.buffer = PieceTable::from_source(SourceBytes::from_vec(bytes), 0, len)?;
        self.change_generation = snapshot.change_generation;
        self.rebuild_line_index();
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
        let bytes = self.buffer.collect_bytes();
        let line = self.line_index.line_for_offset(byte_offset);
        let line_start = self.line_index.line_start(line).unwrap_or(0);
        let before_caret =
            std::str::from_utf8(&bytes[line_start..byte_offset]).map_err(|error| {
                BlitzError::Encoding(format!("document is not valid UTF-8: {error}"))
            })?;
        Ok(CaretPosition {
            line: line + 1,
            column: visual_column(before_caret),
        })
    }

    pub fn visible_lines(&self, first_line: usize, max_lines: usize) -> Vec<VisibleLine> {
        let bytes = self.buffer.collect_bytes();
        (first_line..first_line.saturating_add(max_lines))
            .filter_map(|zero_based_line| {
                let range = self.line_index.line_range(zero_based_line, bytes.len())?;
                let text =
                    String::from_utf8_lossy(trim_newline_bytes(&bytes[range.clone()])).into_owned();
                Some(VisibleLine {
                    number: zero_based_line + 1,
                    byte_range: range,
                    text,
                })
            })
            .collect()
    }

    pub fn save(&mut self) -> Result<()> {
        let path = self.path.clone().ok_or(BlitzError::MissingSavePath)?;
        let saved_len = self.write_to_path(&path, self.encoding, self.line_ending)?;
        self.original_file_len = saved_len;
        self.saved_generation = Some(self.change_generation);
        self.sync_dirty_flag();
        Ok(())
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

    fn commit_successful_edit(&mut self, previous: Vec<u8>) {
        self.undo_stack.push(UndoSnapshot {
            bytes: previous,
            change_generation: self.change_generation,
        });
        if self.undo_stack.len() > UNDO_LIMIT {
            self.undo_stack.remove(0);
        }
        self.change_generation = self.change_generation.saturating_add(1);
        self.rebuild_line_index();
        self.sync_dirty_flag();
    }

    fn sync_dirty_flag(&mut self) {
        self.dirty = self.saved_generation != Some(self.change_generation);
    }

    fn rebuild_line_index(&mut self) {
        self.line_index = LineIndex::build(&self.buffer.collect_bytes());
    }

    fn write_to_path(
        &self,
        path: &Path,
        encoding: TextEncoding,
        line_ending: LineEnding,
    ) -> Result<u64> {
        let normalized = normalize_line_endings(&self.text_lossy(), line_ending);
        let bytes = encode_from_utf8(&normalized, encoding)?;
        let temporary_path = temporary_save_path(path);
        {
            let mut file =
                File::create(&temporary_path).map_err(|error| io_path(&temporary_path, error))?;
            file.write_all(&bytes)
                .map_err(|error| io_path(&temporary_path, error))?;
            file.sync_all()
                .map_err(|error| io_path(&temporary_path, error))?;
        }
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
        Ok(bytes.len() as u64)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextEdit {
    pub range: Range<usize>,
    pub replacement: String,
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

fn temporary_save_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("untitled");
    path.with_file_name(format!(".{file_name}.blitzpad-tmp"))
}

fn trim_newline_bytes(bytes: &[u8]) -> &[u8] {
    if let Some(stripped) = bytes.strip_suffix(b"\r\n") {
        stripped
    } else if let Some(stripped) = bytes.strip_suffix(b"\n") {
        stripped
    } else if let Some(stripped) = bytes.strip_suffix(b"\r") {
        stripped
    } else {
        bytes
    }
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
