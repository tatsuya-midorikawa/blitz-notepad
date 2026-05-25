use std::fs;
use std::io::Write;

use blitz_notepad::document::{LoadMode, TextEdit, MMAP_THRESHOLD_BYTES};
use blitz_notepad::encoding::{decode_to_utf8, detect_encoding, encode_from_utf8, TextEncoding};
use blitz_notepad::line_index::{detect_line_ending, LineEnding};
use blitz_notepad::Document;
use tempfile::tempdir;

#[test]
fn new_untitled_document_is_clean_until_edited() {
    let mut document = Document::new_untitled();

    assert!(!document.is_dirty());

    document.insert_text(0, "x").expect("insert");

    assert!(document.is_dirty());
}

#[test]
fn japanese_input_round_trips_as_utf8() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("japanese.txt");
    let mut document = Document::new_untitled();

    document
        .insert_text(0, "日本語のメモ")
        .expect("insert Japanese");
    document
        .save_as(&path, TextEncoding::Utf8, LineEnding::Lf)
        .expect("save");

    let reopened = Document::open(&path).expect("open");
    assert_eq!(reopened.text_lossy(), "日本語のメモ");
    assert_eq!(reopened.encoding(), TextEncoding::Utf8);
}

#[test]
fn emoji_input_round_trips_as_utf8() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("emoji.txt");
    let mut document = Document::new_untitled();

    document.insert_text(0, "😐😀").expect("insert emoji");
    document
        .save_as(&path, TextEncoding::Utf8, LineEnding::Lf)
        .expect("save");

    let reopened = Document::open(&path).expect("open");
    assert_eq!(reopened.text_lossy(), "😐😀");
    assert_eq!(reopened.encoding(), TextEncoding::Utf8);
}

#[test]
fn detects_utf16_and_ansi_samples() {
    let mut utf16 = vec![0xFF, 0xFE];
    for code_unit in "日本語".encode_utf16() {
        utf16.extend_from_slice(&code_unit.to_le_bytes());
    }
    assert_eq!(detect_encoding(&utf16), TextEncoding::Utf16Le);
    assert_eq!(
        decode_to_utf8(&utf16, TextEncoding::Utf16Le).expect("utf16"),
        "日本語"
    );

    let ansi = encode_from_utf8("日本語", TextEncoding::Ansi).expect("shift-jis");
    assert_eq!(detect_encoding(&ansi), TextEncoding::Ansi);
    assert_eq!(
        decode_to_utf8(&ansi, TextEncoding::Ansi).expect("ansi"),
        "日本語"
    );
}

#[test]
fn detects_all_required_line_endings() {
    assert_eq!(detect_line_ending(b"a\r\nb\r\n"), LineEnding::CrLf);
    assert_eq!(detect_line_ending(b"a\nb\n"), LineEnding::Lf);
    assert_eq!(detect_line_ending(b"a\rb\r"), LineEnding::Cr);
}

#[test]
fn large_file_uses_memory_map_without_crashing() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("large.txt");
    let mut file = fs::File::create(&path).expect("create");
    for _ in 0..1024 {
        writeln!(file, "sample text").expect("seed");
    }
    file.set_len(MMAP_THRESHOLD_BYTES + 1)
        .expect("sparse large file");
    drop(file);

    let document = Document::open(&path).expect("open large file");
    assert_eq!(document.load_mode(), LoadMode::MemoryMapped);
    assert!(document.original_file_len() > MMAP_THRESHOLD_BYTES);
    assert_eq!(document.visible_lines(0, 1)[0].text, "sample text");
}

#[test]
fn visible_lines_cap_very_long_rows_for_virtual_rendering() {
    let mut document = Document::new_untitled();
    document
        .insert_text(0, &"x".repeat(64 * 1024))
        .expect("insert long line");

    let line = document
        .visible_lines(0, 1)
        .into_iter()
        .next()
        .expect("visible line");

    assert!(line.text.len() <= 16 * 1024);
    assert!(line.byte_range.len() <= 16 * 1024);
}

#[test]
fn line_index_updates_when_edit_joins_lf_into_crlf() {
    let mut document = Document::new_untitled();
    document.insert_text(0, "a\nb").expect("insert");

    document.insert_text(1, "\r").expect("insert cr");

    assert_eq!(document.text_lossy(), "a\r\nb");
    assert_eq!(document.line_count(), 2);
    assert_eq!(document.visible_lines(0, 2)[0].text, "a");
    assert_eq!(document.visible_lines(0, 2)[1].text, "b");

    assert!(document.undo().expect("undo"));
    assert_eq!(document.text_lossy(), "a\nb");
    assert_eq!(document.line_count(), 2);
}

#[test]
fn multi_cursor_edits_apply_from_document_end() {
    let mut document = Document::new_untitled();
    document.insert_text(0, "aa bb cc").expect("insert");
    document
        .apply_edits_from_end(&[
            TextEdit {
                range: 0..2,
                replacement: "AA".to_owned(),
            },
            TextEdit {
                range: 6..8,
                replacement: "CC".to_owned(),
            },
        ])
        .expect("multi edit");

    assert_eq!(document.text_lossy(), "AA bb CC");
}

#[test]
fn save_as_normalizes_line_endings_and_utf8_bom() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("saved.txt");
    let mut document = Document::new_untitled();
    document.insert_text(0, "a\nb\rc\r\n").expect("insert");
    document
        .save_as(&path, TextEncoding::Utf8Bom, LineEnding::CrLf)
        .expect("save");

    assert!(!document.is_dirty());
    assert_eq!(document.file_name(), "saved.txt");
    assert_eq!(
        fs::read(&path).expect("read"),
        b"\xEF\xBB\xBFa\r\nb\r\nc\r\n"
    );
}

#[test]
fn streaming_utf8_save_normalizes_across_piece_boundaries() {
    let directory = tempdir().expect("tempdir");
    let source_path = directory.path().join("source.txt");
    let saved_path = directory.path().join("saved.txt");
    fs::write(&source_path, b"a\nb").expect("seed");
    let mut document = Document::open(&source_path).expect("open");

    document.insert_text(1, "\r").expect("insert cr");
    document
        .save_as(&saved_path, TextEncoding::Utf8, LineEnding::CrLf)
        .expect("save");

    assert_eq!(fs::read(saved_path).expect("read"), b"a\r\nb");
}

#[test]
fn streaming_utf8_save_normalizes_trailing_cr() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("trailing-cr.txt");
    let mut document = Document::new_untitled();
    document.insert_text(0, "a\r").expect("insert");

    document
        .save_as(&path, TextEncoding::Utf8, LineEnding::Lf)
        .expect("save");

    assert_eq!(fs::read(path).expect("read"), b"a\n");
}

#[test]
fn undo_restores_previous_document_text() {
    let mut document = Document::new_untitled();
    document.insert_text(0, "alpha").expect("insert");
    document.insert_text(5, " beta").expect("insert");

    assert!(document.undo_available());
    assert!(document.undo().expect("undo"));
    assert_eq!(document.text_lossy(), "alpha");
    assert!(document.is_dirty());
}

#[test]
fn undo_back_to_saved_content_clears_dirty_flag() {
    let directory = tempdir().expect("tempdir");
    let path = directory.path().join("undo-clean.txt");
    let mut document = Document::new_untitled();
    document.insert_text(0, "alpha").expect("insert");
    document
        .save_as(&path, TextEncoding::Utf8, LineEnding::Lf)
        .expect("save");

    document.insert_text(5, " beta").expect("insert");
    assert!(document.is_dirty());
    assert!(document.undo().expect("undo"));

    assert_eq!(document.text_lossy(), "alpha");
    assert!(!document.is_dirty());
}

#[test]
fn caret_position_counts_tabs_as_visual_columns() {
    let mut document = Document::new_untitled();
    document.insert_text(0, "a\tb").expect("insert");

    assert_eq!(document.caret_position(2).expect("caret").column, 9);
    assert_eq!(document.caret_position(3).expect("caret").column, 10);
}

#[test]
fn caret_position_tracks_multiline_locations() {
    let mut document = Document::new_untitled();
    document.insert_text(0, "one\ntwo\nthree").expect("insert");

    let caret = document.caret_position("one\ntw".len()).expect("caret");
    assert_eq!(caret.line, 2);
    assert_eq!(caret.column, 3);
}
