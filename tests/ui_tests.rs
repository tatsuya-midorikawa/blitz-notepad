use blitz_notepad::encoding::TextEncoding;
use blitz_notepad::line_index::LineEnding;
use blitz_notepad::settings::EditorSettings;
use blitz_notepad::ui::{default_ui_state, FontDialogModel, MENU_BAR};
use blitz_notepad::BlitzApp;

#[test]
fn ui_snapshot_matches_notepad_shell() {
    let settings = EditorSettings::default();
    let state = default_ui_state(&settings);

    assert_eq!(MENU_BAR, ["File", "Edit", "Format", "View", "Help"]);
    assert_eq!(
        state.shell_snapshot(),
        vec![
            "*Untitled - Notepad".to_owned(),
            "File Edit Format View Help".to_owned(),
            "Ln 1, Col 1 | 100% | Windows (CRLF) | UTF-8".to_owned(),
        ]
    );
}

#[test]
fn app_ui_handles_japanese_input_without_mojibake() {
    let mut app = BlitzApp::default();
    app.insert_text("日本語").expect("insert");
    let state = app.ui_state().expect("ui state");

    assert_eq!(app.document().text_lossy(), "日本語");
    assert_eq!(state.status_cells()[0], "Ln 1, Col 4");
}

#[test]
fn title_and_status_reflect_save_zoom_and_visibility() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("note.txt");
    let mut app = BlitzApp::default();

    app.insert_text("one\ntwo").expect("insert");
    app.set_caret_offset("one\nt".len()).expect("caret");
    app.settings_mut().set_zoom_percent(150);
    app.document_mut()
        .save_as(&path, TextEncoding::Utf8, LineEnding::Lf)
        .expect("save");

    let state = app.ui_state().expect("ui");
    assert_eq!(state.title(), "note.txt - Notepad");
    assert_eq!(
        state.status_cells(),
        vec!["Ln 2, Col 2", "150%", "Unix (LF)", "UTF-8"]
    );

    app.toggle_status_bar();
    assert!(app.ui_state().expect("ui").status_cells().is_empty());
}

#[test]
fn file_menu_text_and_shortcuts_match_reference() {
    let state = default_ui_state(&EditorSettings::default());
    let labels = state
        .menu("File")
        .expect("file menu")
        .into_iter()
        .map(|item| (item.label, item.shortcut))
        .collect::<Vec<_>>();

    assert_eq!(
        labels,
        vec![
            ("New", Some("Ctrl+N")),
            ("New Window", Some("Ctrl+Shift+N")),
            ("Open...", Some("Ctrl+O")),
            ("Save", Some("Ctrl+S")),
            ("Save As...", Some("Ctrl+Shift+S")),
            ("Page Setup...", None),
            ("Print...", Some("Ctrl+P")),
            ("Exit", None),
        ]
    );
}

#[test]
fn all_menu_text_and_shortcuts_match_reference_order() {
    let state = default_ui_state(&EditorSettings::default());

    let edit = labels_and_shortcuts(&state.menu("Edit").expect("edit menu"));
    assert_eq!(
        edit,
        vec![
            ("Undo", Some("Ctrl+Z")),
            ("Cut", Some("Ctrl+X")),
            ("Copy", Some("Ctrl+C")),
            ("Paste", Some("Ctrl+V")),
            ("Delete", Some("Del")),
            ("Search with Bing...", Some("Ctrl+E")),
            ("Find...", Some("Ctrl+F")),
            ("Find Next", Some("F3")),
            ("Find Previous", Some("Shift+F3")),
            ("Replace...", Some("Ctrl+H")),
            ("Go To...", Some("Ctrl+G")),
            ("Select All", Some("Ctrl+A")),
            ("Time/Date", Some("F5")),
        ]
    );

    let format = labels_and_shortcuts(&state.menu("Format").expect("format menu"));
    assert_eq!(format, vec![("Word Wrap", None), ("Font...", None)]);

    let view = state.menu("View").expect("view menu");
    assert_eq!(view[0].label, "Zoom");
    assert_eq!(
        labels_and_shortcuts(&view[0].children),
        vec![
            ("Zoom In", Some("Ctrl+Plus")),
            ("Zoom Out", Some("Ctrl+Minus")),
            ("Restore Default Zoom", Some("Ctrl+0")),
        ]
    );
    assert_eq!(view[1].label, "Status Bar");

    let help = labels_and_shortcuts(&state.menu("Help").expect("help menu"));
    assert_eq!(
        help,
        vec![
            ("View Help", None),
            ("Send Feedback", None),
            ("About Notepad", None)
        ]
    );
}

#[test]
fn edit_menu_enables_items_from_current_state() {
    let mut state = default_ui_state(&EditorSettings::default());
    let edit_menu = state.menu("Edit").expect("edit menu");
    assert!(
        !edit_menu
            .iter()
            .find(|item| item.label == "Cut")
            .expect("cut")
            .enabled
    );
    assert!(
        edit_menu
            .iter()
            .find(|item| item.label == "Go To...")
            .expect("go to")
            .enabled
    );
    assert!(
        !edit_menu
            .iter()
            .find(|item| item.label == "Delete")
            .expect("delete")
            .enabled
    );

    state.selection_bytes = "日本語".len();
    state.can_delete_forward = true;
    state.undo_available = true;
    state.clipboard_has_text = true;
    state.word_wrap = true;
    let edit_menu = state.menu("Edit").expect("edit menu");
    assert!(
        edit_menu
            .iter()
            .find(|item| item.label == "Cut")
            .expect("cut")
            .enabled
    );
    assert!(
        edit_menu
            .iter()
            .find(|item| item.label == "Delete")
            .expect("delete")
            .enabled
    );
    assert!(
        edit_menu
            .iter()
            .find(|item| item.label == "Paste")
            .expect("paste")
            .enabled
    );
    assert!(
        !edit_menu
            .iter()
            .find(|item| item.label == "Go To...")
            .expect("go to")
            .enabled
    );
}

#[test]
fn delete_forward_removes_next_character_without_selection() {
    let mut app = BlitzApp::default();
    app.insert_text("a日本").expect("insert");
    app.set_caret_offset(1).expect("caret");

    assert!(app.ui_state().expect("ui").can_delete_forward);
    assert!(app.delete_forward().expect("delete"));
    assert_eq!(app.document().text_lossy(), "a本");
}

#[test]
fn format_view_help_and_font_dialog_are_present() {
    let mut state = default_ui_state(&EditorSettings::default());
    state.word_wrap = true;

    let format_menu = state.menu("Format").expect("format menu");
    assert_eq!(format_menu[0].label, "Word Wrap");
    assert!(format_menu[0].checked);
    assert_eq!(format_menu[1].label, "Font...");

    let view_menu = state.menu("View").expect("view menu");
    assert_eq!(view_menu[0].label, "Zoom");
    assert_eq!(view_menu[0].children[0].label, "Zoom In");
    assert_eq!(view_menu[0].children[1].shortcut, Some("Ctrl+Minus"));
    assert_eq!(view_menu[1].label, "Status Bar");

    let help_menu = state.menu("Help").expect("help menu");
    assert_eq!(help_menu[0].label, "View Help");
    assert_eq!(help_menu[2].label, "About Notepad");

    let model = FontDialogModel::from_settings(&EditorSettings::default());
    assert_eq!(model.sample, "AaBbYyZz");
    assert!(model.styles.contains(&"Regular"));
    assert!(model.styles.contains(&"Bold Italic"));
    assert!(model.sizes.contains(&11));
    assert_eq!(model.script, "Western");
}

fn labels_and_shortcuts(
    items: &[blitz_notepad::ui::MenuItem],
) -> Vec<(&'static str, Option<&'static str>)> {
    items
        .iter()
        .map(|item| (item.label, item.shortcut))
        .collect()
}
