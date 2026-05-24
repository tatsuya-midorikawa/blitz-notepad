use std::fs;
use std::io::Write;

use blitz_notepad::document::{LoadMode, TextEdit, MMAP_THRESHOLD_BYTES};
use blitz_notepad::encoding::{decode_to_utf8, detect_encoding, encode_from_utf8, TextEncoding};
use blitz_notepad::line_index::{detect_line_ending, LineEnding};
use blitz_notepad::Document;
use tempfile::tempdir;

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
