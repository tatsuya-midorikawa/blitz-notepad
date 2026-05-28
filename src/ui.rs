use crate::encoding::TextEncoding;
use crate::line_index::LineEnding;
use crate::settings::{EditorSettings, FontStyle};

pub const APP_TITLE_SUFFIX: &str = "Notepad";
pub const MENU_BAR: [&str; 5] = ["File", "Edit", "Format", "View", "Help"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotepadUiState {
    pub file_name: String,
    pub dirty: bool,
    pub caret_line: usize,
    pub caret_column: usize,
    pub zoom_percent: u16,
    pub line_ending: LineEnding,
    pub encoding: TextEncoding,
    pub selection_bytes: usize,
    pub can_delete_forward: bool,
    pub undo_available: bool,
    pub clipboard_has_text: bool,
    pub word_wrap: bool,
    pub status_bar_visible: bool,
}

impl NotepadUiState {
    pub fn title(&self) -> String {
        let dirty_prefix = if self.dirty { "*" } else { "" };
        format!("{dirty_prefix}{} - {APP_TITLE_SUFFIX}", self.file_name)
    }

    pub fn status_cells(&self) -> Vec<String> {
        if !self.status_bar_visible {
            return Vec::new();
        }
        vec![
            format!("Ln {}, Col {}", self.caret_line, self.caret_column),
            format!("{}%", self.zoom_percent),
            self.line_ending.label().to_owned(),
            self.encoding.label().to_owned(),
        ]
    }

    pub fn menu(&self, top_menu: &str) -> Option<Vec<MenuItem>> {
        match top_menu {
            "File" => Some(file_menu()),
            "Edit" => Some(edit_menu(self)),
            "Format" => Some(format_menu(self)),
            "View" => Some(view_menu(self)),
            "Help" => Some(help_menu()),
            _ => None,
        }
    }

    pub fn shell_snapshot(&self) -> Vec<String> {
        vec![
            self.title(),
            MENU_BAR.join(" "),
            self.status_cells().join(" | "),
        ]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuItem {
    pub label: &'static str,
    pub shortcut: Option<&'static str>,
    pub enabled: bool,
    pub checked: bool,
    pub separator_after: bool,
    pub children: Vec<MenuItem>,
}

impl MenuItem {
    fn command(label: &'static str, shortcut: Option<&'static str>) -> Self {
        Self {
            label,
            shortcut,
            enabled: true,
            checked: false,
            separator_after: false,
            children: Vec::new(),
        }
    }

    fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }

    fn separator_after(mut self) -> Self {
        self.separator_after = true;
        self
    }

    fn submenu(label: &'static str, children: Vec<MenuItem>) -> Self {
        Self {
            label,
            shortcut: None,
            enabled: true,
            checked: false,
            separator_after: false,
            children,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FontDialogModel {
    pub font: String,
    pub style: FontStyle,
    pub size: u16,
    pub script: String,
    pub sample: &'static str,
    pub styles: Vec<&'static str>,
    pub sizes: Vec<u16>,
}

impl FontDialogModel {
    pub fn from_settings(settings: &EditorSettings) -> Self {
        Self {
            font: settings.font_family.clone(),
            style: settings.font_style,
            size: settings.font_size,
            script: settings.script.clone(),
            sample: "AaBbYyZz",
            styles: vec!["Regular", "Italic", "Bold", "Bold Italic"],
            sizes: vec![8, 9, 10, 11, 12, 14, 16, 18, 20, 22, 24, 26, 28, 36, 48, 72],
        }
    }
}

pub fn default_ui_state(settings: &EditorSettings) -> NotepadUiState {
    NotepadUiState {
        file_name: "Untitled".to_owned(),
        dirty: false,
        caret_line: 1,
        caret_column: 1,
        zoom_percent: settings.zoom_percent,
        line_ending: LineEnding::CrLf,
        encoding: TextEncoding::Utf8,
        selection_bytes: 0,
        can_delete_forward: false,
        undo_available: false,
        clipboard_has_text: false,
        word_wrap: settings.word_wrap,
        status_bar_visible: settings.status_bar_visible,
    }
}

fn file_menu() -> Vec<MenuItem> {
    vec![
        MenuItem::command("New", Some("Ctrl+N")),
        MenuItem::command("New Window", Some("Ctrl+Shift+N")),
        MenuItem::command("Open...", Some("Ctrl+O")),
        MenuItem::command("Save", Some("Ctrl+S")),
        MenuItem::command("Save As...", Some("Ctrl+Shift+S")).separator_after(),
        MenuItem::command("Print...", Some("Ctrl+P")).separator_after(),
        MenuItem::command("Exit", None),
    ]
}

fn edit_menu(state: &NotepadUiState) -> Vec<MenuItem> {
    let has_selection = state.selection_bytes > 0;
    vec![
        maybe_disabled(
            MenuItem::command("Undo", Some("Ctrl+Z")),
            state.undo_available,
        )
        .separator_after(),
        maybe_disabled(MenuItem::command("Cut", Some("Ctrl+X")), has_selection),
        maybe_disabled(MenuItem::command("Copy", Some("Ctrl+C")), has_selection),
        maybe_disabled(
            MenuItem::command("Paste", Some("Ctrl+V")),
            state.clipboard_has_text,
        ),
        maybe_disabled(
            MenuItem::command("Delete", Some("Del")),
            has_selection || state.can_delete_forward,
        )
        .separator_after(),
        maybe_disabled(
            MenuItem::command("Search with Bing...", Some("Ctrl+E")),
            has_selection,
        )
        .separator_after(),
        MenuItem::command("Find...", Some("Ctrl+F")),
        MenuItem::command("Find Next", Some("F3")),
        MenuItem::command("Find Previous", Some("Shift+F3")),
        MenuItem::command("Replace...", Some("Ctrl+H")),
        maybe_disabled(
            MenuItem::command("Go To...", Some("Ctrl+G")),
            !state.word_wrap,
        )
        .separator_after(),
        MenuItem::command("Select All", Some("Ctrl+A")),
        MenuItem::command("Time/Date", Some("F5")),
    ]
}

fn format_menu(state: &NotepadUiState) -> Vec<MenuItem> {
    vec![
        MenuItem::command("Word Wrap", None).checked(state.word_wrap),
        MenuItem::command("Font...", None),
    ]
}

fn view_menu(state: &NotepadUiState) -> Vec<MenuItem> {
    vec![
        MenuItem::submenu(
            "Zoom",
            vec![
                MenuItem::command("Zoom In", Some("Ctrl+Plus")),
                MenuItem::command("Zoom Out", Some("Ctrl+Minus")),
                MenuItem::command("Restore Default Zoom", Some("Ctrl+0")),
            ],
        ),
        MenuItem::command("Status Bar", None).checked(state.status_bar_visible),
    ]
}

fn help_menu() -> Vec<MenuItem> {
    vec![
        MenuItem::command("View Help", None),
        MenuItem::command("Send Feedback", None).separator_after(),
        MenuItem::command("About Notepad", None),
    ]
}

fn maybe_disabled(item: MenuItem, enabled: bool) -> MenuItem {
    if enabled {
        item
    } else {
        item.disabled()
    }
}
