use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use memmap2::Mmap;

use crate::{BlitzError, Result};

#[derive(Clone)]
pub enum SourceBytes {
    Heap(Arc<[u8]>),
    Mapped(Arc<Mmap>),
}

impl SourceBytes {
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self::Heap(Arc::from(bytes.into_boxed_slice()))
    }

    pub fn from_mmap(mmap: Mmap) -> Self {
        Self::Mapped(Arc::new(mmap))
    }

    pub fn as_slice(&self) -> &[u8] {
        match self {
            SourceBytes::Heap(bytes) => bytes,
            SourceBytes::Mapped(mmap) => mmap,
        }
    }

    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_mapped(&self) -> bool {
        matches!(self, SourceBytes::Mapped(_))
    }
}

impl fmt::Debug for SourceBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceBytes")
            .field("len", &self.len())
            .field("mapped", &self.is_mapped())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BufferKind {
    Original,
    Add,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Piece {
    source: BufferKind,
    start: usize,
    len: usize,
}

#[derive(Clone, Debug)]
pub struct PieceTable {
    original: SourceBytes,
    add: Vec<u8>,
    pieces: Vec<Piece>,
    len: usize,
}

impl PieceTable {
    pub fn new() -> Self {
        Self::from_text("")
    }

    pub fn from_text(text: &str) -> Self {
        Self::from_source(
            SourceBytes::from_vec(text.as_bytes().to_vec()),
            0,
            text.len(),
        )
        .expect("text source bounds are valid")
    }

    pub fn from_source(original: SourceBytes, start: usize, len: usize) -> Result<Self> {
        if start + len > original.len() {
            return Err(BlitzError::InvalidRange {
                start,
                end: start + len,
                len: original.len(),
            });
        }

        let pieces = if len == 0 {
            Vec::new()
        } else {
            vec![Piece {
                source: BufferKind::Original,
                start,
                len,
            }]
        };

        Ok(Self {
            original,
            add: Vec::new(),
            pieces,
            len,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn original_is_mapped(&self) -> bool {
        self.original.is_mapped()
    }

    pub fn collect_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.len());
        for piece in &self.pieces {
            bytes.extend_from_slice(self.piece_bytes(piece));
        }
        bytes
    }

    pub fn for_each_chunk<E>(
        &self,
        mut visit: impl FnMut(&[u8]) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        for piece in &self.pieces {
            visit(self.piece_bytes(piece))?;
        }
        Ok(())
    }

    pub fn bytes_range(&self, range: Range<usize>) -> Result<Vec<u8>> {
        self.ensure_range_bounds(&range)?;
        let mut bytes = Vec::with_capacity(range.end - range.start);
        let mut cursor = 0usize;

        for piece in &self.pieces {
            let piece_start = cursor;
            let piece_end = cursor + piece.len;
            cursor = piece_end;

            if piece_end <= range.start || piece_start >= range.end {
                continue;
            }

            let overlap_start = range.start.max(piece_start);
            let overlap_end = range.end.min(piece_end);
            let local_start = overlap_start - piece_start;
            let local_end = overlap_end - piece_start;
            bytes.extend_from_slice(&self.piece_bytes(piece)[local_start..local_end]);
        }

        Ok(bytes)
    }

    pub fn byte_at(&self, offset: usize) -> Result<Option<u8>> {
        let len = self.len();
        if offset > len {
            return Err(BlitzError::InvalidRange {
                start: offset,
                end: offset,
                len,
            });
        }
        if offset == len {
            return Ok(None);
        }

        let (piece_index, inner_offset) = self.piece_position(offset)?;
        Ok(self
            .pieces
            .get(piece_index)
            .map(|piece| self.piece_bytes(piece)[inner_offset]))
    }

    pub fn to_text_lossy(&self) -> String {
        String::from_utf8_lossy(&self.collect_bytes()).into_owned()
    }

    pub fn insert_str(&mut self, byte_offset: usize, text: &str) -> Result<()> {
        self.ensure_char_boundary(byte_offset)?;
        if text.is_empty() {
            return Ok(());
        }

        let add_start = self.add.len();
        self.add.extend_from_slice(text.as_bytes());
        let inserted_piece = Piece {
            source: BufferKind::Add,
            start: add_start,
            len: text.len(),
        };
        let (piece_index, inner_offset) = self.piece_position(byte_offset)?;

        if piece_index == self.pieces.len() {
            self.pieces.push(inserted_piece);
        } else {
            let piece = self.pieces[piece_index];
            let mut replacement = Vec::with_capacity(3);
            if inner_offset > 0 {
                replacement.push(Piece {
                    source: piece.source,
                    start: piece.start,
                    len: inner_offset,
                });
            }
            replacement.push(inserted_piece);
            if inner_offset < piece.len {
                replacement.push(Piece {
                    source: piece.source,
                    start: piece.start + inner_offset,
                    len: piece.len - inner_offset,
                });
            }
            self.pieces.splice(piece_index..=piece_index, replacement);
        }
        self.merge_adjacent();
        self.len += text.len();
        Ok(())
    }

    pub fn delete_range(&mut self, range: Range<usize>) -> Result<()> {
        self.ensure_range(&range)?;
        if range.is_empty() {
            return Ok(());
        }

        let mut new_pieces = Vec::with_capacity(self.pieces.len());
        let mut cursor = 0usize;
        for piece in &self.pieces {
            let piece_start = cursor;
            let piece_end = cursor + piece.len;
            cursor = piece_end;

            if piece_end <= range.start || piece_start >= range.end {
                new_pieces.push(*piece);
                continue;
            }

            if range.start > piece_start {
                new_pieces.push(Piece {
                    source: piece.source,
                    start: piece.start,
                    len: range.start - piece_start,
                });
            }

            if range.end < piece_end {
                let keep_start_in_piece = range.end - piece_start;
                new_pieces.push(Piece {
                    source: piece.source,
                    start: piece.start + keep_start_in_piece,
                    len: piece_end - range.end,
                });
            }
        }

        self.pieces = new_pieces;
        self.merge_adjacent();
        self.len -= range.len();
        Ok(())
    }

    pub fn replace_range(&mut self, range: Range<usize>, text: &str) -> Result<()> {
        let start = range.start;
        self.delete_range(range)?;
        self.insert_str(start, text)
    }

    fn piece_bytes(&self, piece: &Piece) -> &[u8] {
        let source = match piece.source {
            BufferKind::Original => self.original.as_slice(),
            BufferKind::Add => &self.add,
        };
        &source[piece.start..piece.start + piece.len]
    }

    fn piece_position(&self, byte_offset: usize) -> Result<(usize, usize)> {
        let len = self.len();
        if byte_offset > len {
            return Err(BlitzError::InvalidRange {
                start: byte_offset,
                end: byte_offset,
                len,
            });
        }

        if byte_offset == len {
            return Ok((self.pieces.len(), 0));
        }

        let mut cursor = 0usize;
        for (piece_index, piece) in self.pieces.iter().enumerate() {
            let piece_end = cursor + piece.len;
            if byte_offset < piece_end {
                return Ok((piece_index, byte_offset - cursor));
            }
            cursor = piece_end;
        }
        Ok((self.pieces.len(), 0))
    }

    fn ensure_range_bounds(&self, range: &Range<usize>) -> Result<()> {
        let len = self.len();
        if range.start > range.end || range.end > len {
            Err(BlitzError::InvalidRange {
                start: range.start,
                end: range.end,
                len,
            })
        } else {
            Ok(())
        }
    }

    fn ensure_range(&self, range: &Range<usize>) -> Result<()> {
        self.ensure_range_bounds(range)?;
        self.ensure_char_boundary(range.start)?;
        self.ensure_char_boundary(range.end)
    }

    fn ensure_char_boundary(&self, byte_offset: usize) -> Result<()> {
        let len = self.len();
        if byte_offset > len {
            return Err(BlitzError::InvalidRange {
                start: byte_offset,
                end: byte_offset,
                len,
            });
        }
        if byte_offset == len {
            return Ok(());
        }

        if self
            .byte_at(byte_offset)?
            .is_some_and(|byte| !is_utf8_continuation(byte))
        {
            Ok(())
        } else {
            Err(BlitzError::InvalidCharBoundary {
                offset: byte_offset,
            })
        }
    }

    fn merge_adjacent(&mut self) {
        let mut merged: Vec<Piece> = Vec::with_capacity(self.pieces.len());
        for piece in &self.pieces {
            if let Some(previous) = merged.last_mut() {
                if previous.source == piece.source && previous.start + previous.len == piece.start {
                    previous.len += piece.len;
                    continue;
                }
            }
            merged.push(*piece);
        }
        self.pieces = merged;
    }
}

fn is_utf8_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

impl Default for PieceTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_and_deletes_without_mutating_original() {
        let source = SourceBytes::from_vec(b"hello world".to_vec());
        let mut table = PieceTable::from_source(source, 0, 11).expect("table");
        table.insert_str(5, " 日本語").expect("insert");
        table.delete_range(0..1).expect("delete");
        assert_eq!(table.to_text_lossy(), "ello 日本語 world");
    }

    #[test]
    fn rejects_non_boundary_edit() {
        let mut table = PieceTable::from_text("日本");
        assert!(matches!(
            table.insert_str(1, "x"),
            Err(BlitzError::InvalidCharBoundary { offset: 1 })
        ));
    }

    #[test]
    fn len_tracks_piece_table_edits() {
        let mut table = PieceTable::from_text("abc");

        table.insert_str(1, "日本").expect("insert");
        assert_eq!(table.len(), "a日本bc".len());

        table.delete_range(1.."a日本".len()).expect("delete");
        assert_eq!(table.len(), "abc".len());
        assert_eq!(table.to_text_lossy(), "abc");
    }
}
