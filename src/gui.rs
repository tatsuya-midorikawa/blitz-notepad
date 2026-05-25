use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use ab_glyph::{point, Font, FontArc, FontVec, GlyphId, PxScale, ScaleFont};
use arboard::Clipboard;
use fontdb::{Database, Family, Query};
use minifb::{InputCallback, Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};
use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};

use crate::ui::{MenuItem, NotepadUiState, MENU_BAR};
use crate::{BlitzApp, BlitzError, Result};

const DEFAULT_WIDTH: usize = 960;
const DEFAULT_HEIGHT: usize = 640;
const MIN_WIDTH: usize = 420;
const MIN_HEIGHT: usize = 240;
const MENU_HEIGHT: usize = 30;
const MENU_ITEM_HEIGHT: usize = 26;
const MENU_SEPARATOR_HEIGHT: usize = 7;
const STATUS_HEIGHT: usize = 30;
const SCROLLBAR_WIDTH: usize = 16;
const TEXT_MARGIN_X: usize = 8;
const TEXT_MARGIN_Y: usize = 8;
const EDITOR_LINE_HEIGHT: usize = 24;
const CARET_HEIGHT: usize = 22;
const UI_FONT_SIZE: f32 = 15.0;
const EDITOR_FONT_SIZE: f32 = 18.0;
const WHEEL_LINES: isize = 3;
const HORIZONTAL_WHEEL_BYTES: isize = 96;
const MIN_SCROLL_THUMB: usize = 32;

const COLOR_WINDOW: u32 = 0x00f0f0f0;
const COLOR_TEXT_AREA: u32 = 0x00ffffff;
const COLOR_STATUS: u32 = 0x00f0f0f0;
const COLOR_BORDER: u32 = 0x00d4d4d4;
const COLOR_MENU_OPEN: u32 = 0x00dbeeff;
const COLOR_DROPDOWN: u32 = 0x00f8f8f8;
const COLOR_SCROLLBAR: u32 = 0x00e6e6e6;
const COLOR_SCROLL_THUMB: u32 = 0x00b8b8b8;
const COLOR_TEXT: u32 = 0x00000000;
const COLOR_DISABLED_TEXT: u32 = 0x00808080;
const COLOR_CARET: u32 = 0x00000000;

const FONT_FAMILIES: &[&str] = &[
    "Aptos",
    "Aptos Display",
    "Aptos Text",
    "Yu Gothic UI",
    "Yu Gothic",
    "YuGothic",
    "Hiragino Sans",
    "Hiragino Kaku Gothic ProN",
    "Apple SD Gothic Neo",
    "Segoe UI",
    "Helvetica Neue",
    "Arial Unicode MS",
    "Arial",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuiOptions {
    pub smoke_frames: Option<usize>,
}

impl GuiOptions {
    pub fn interactive() -> Self {
        Self { smoke_frames: None }
    }

    pub fn smoke_test(frames: usize) -> Self {
        Self {
            smoke_frames: Some(frames.max(1)),
        }
    }
}

pub fn run_window(app: BlitzApp, options: GuiOptions) -> Result<()> {
    let mut app = app;
    let mut gui_state = GuiState::new()?;
    let mut window = Window::new(
        &app.ui_state()?.title(),
        DEFAULT_WIDTH,
        DEFAULT_HEIGHT,
        WindowOptions {
            resize: true,
            ..WindowOptions::default()
        },
    )
    .map_err(|error| BlitzError::Window(error.to_string()))?;

    let input_queue = Rc::new(RefCell::new(Vec::new()));
    window.set_input_callback(Box::new(TextInput::new(Rc::clone(&input_queue))));
    let mut remaining_frames = options.smoke_frames;
    let mut frame_cache: Option<CachedFrame> = None;
    let mut last_title = String::new();

    while window.is_open() && !window.is_key_down(Key::Escape) {
        app.set_clipboard_has_text(!gui_state.clipboard.is_empty());
        if handle_mouse(&window, &mut app, &mut gui_state)? {
            break;
        }
        handle_keys(&window, &mut app, &mut gui_state)?;
        handle_text_input(&input_queue, &window, &mut app, &mut gui_state)?;
        poll_save_job(&mut app, &mut gui_state);

        let (width, height) = window.get_size();
        let width = width.max(MIN_WIDTH);
        let height = height.max(MIN_HEIGHT);
        handle_scroll_wheel(&window, &app, &mut gui_state, width, height)?;
        gui_state.follow_caret_if_moved(&app, width, height)?;
        app.document().refresh_line_index();
        let state = app.ui_state()?;
        let signature = frame_signature(&app, &state, width, height, &gui_state);
        if frame_cache
            .as_ref()
            .is_none_or(|cache| cache.signature != signature)
        {
            frame_cache = Some(CachedFrame {
                frame: render_frame_with_state(&app, &state, width, height, &gui_state)?,
                signature,
            });
        }
        let frame = &frame_cache
            .as_ref()
            .expect("frame cache is initialized")
            .frame;
        window
            .update_with_buffer(&frame.pixels, frame.width, frame.height)
            .map_err(|error| BlitzError::Window(error.to_string()))?;
        let title = state.title();
        if title != last_title {
            window.set_title(&title);
            last_title = title;
        }

        if let Some(frames) = remaining_frames.as_mut() {
            *frames = frames.saturating_sub(1);
            if *frames == 0 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    app.settings().save()
}

struct GuiState {
    active_menu: Option<usize>,
    dialog: Option<DialogState>,
    last_search: Option<SearchSpec>,
    clipboard: String,
    mouse_was_down: bool,
    first_visible_line: usize,
    horizontal_offset: usize,
    last_caret_offset: usize,
    scroll_drag: Option<ScrollDrag>,
    save_job: Option<SaveJob>,
    fonts: FontStack,
}

impl GuiState {
    fn new() -> Result<Self> {
        let fonts = FontStack::load()?;
        Ok(Self {
            active_menu: None,
            dialog: None,
            last_search: None,
            clipboard: String::new(),
            mouse_was_down: false,
            first_visible_line: 0,
            horizontal_offset: 0,
            last_caret_offset: 0,
            scroll_drag: None,
            save_job: None,
            fonts,
        })
    }

    fn set_message(&mut self, _message: impl Into<String>) {}

    fn reset_scroll(&mut self, app: &BlitzApp) {
        self.first_visible_line = 0;
        self.horizontal_offset = 0;
        self.last_caret_offset = app.caret_offset();
    }

    fn scroll_vertical(&mut self, app: &BlitzApp, delta_lines: isize, visible_lines: usize) {
        let max_first_line = max_first_visible_line(app, visible_lines);
        self.first_visible_line =
            offset_with_delta(self.first_visible_line, delta_lines).min(max_first_line);
        self.clamp_horizontal(app, visible_lines);
    }

    fn scroll_horizontal(&mut self, app: &BlitzApp, delta_bytes: isize, visible_lines: usize) {
        self.horizontal_offset = offset_with_delta(self.horizontal_offset, delta_bytes);
        self.clamp_horizontal(app, visible_lines);
    }

    fn clamp_horizontal(&mut self, app: &BlitzApp, visible_lines: usize) {
        self.horizontal_offset = self.horizontal_offset.min(max_visible_line_len(
            app,
            self.first_visible_line,
            visible_lines,
        ));
    }

    fn follow_caret_if_moved(&mut self, app: &BlitzApp, width: usize, height: usize) -> Result<()> {
        if self.last_caret_offset == app.caret_offset() {
            return Ok(());
        }

        if let Some(metrics) = editor_metrics(&app.ui_state()?, width, height) {
            let caret_line = app.document().line_for_offset(app.caret_offset())?;
            if caret_line < self.first_visible_line {
                self.first_visible_line = caret_line;
            } else if caret_line >= self.first_visible_line + metrics.visible_line_count {
                self.first_visible_line = caret_line
                    .saturating_sub(metrics.visible_line_count)
                    .saturating_add(1);
            }
            self.first_visible_line = self
                .first_visible_line
                .min(max_first_visible_line(app, metrics.visible_line_count));

            let visible_line =
                app.document()
                    .visible_lines_at(caret_line, 1, self.horizontal_offset);
            if let Some(line) = visible_line.first() {
                if app.caret_offset() < line.byte_range.start
                    || app.caret_offset() > line.byte_range.end
                {
                    let line_start = app.document().line_start(caret_line).unwrap_or(0);
                    self.horizontal_offset = app.caret_offset().saturating_sub(line_start);
                }
            }
            self.clamp_horizontal(app, metrics.visible_line_count);
        }

        self.last_caret_offset = app.caret_offset();
        Ok(())
    }
}

struct SaveJob {
    receiver: Receiver<SaveJobResult>,
}

struct SaveJobResult {
    generation: u64,
    result: Result<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScrollDrag {
    Vertical { grab_offset: usize },
    Horizontal { grab_offset: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrameSignature {
    width: usize,
    height: usize,
    ui_state: NotepadUiState,
    document_line_count: usize,
    active_menu: Option<usize>,
    dialog: Option<DialogState>,
    first_visible_line: usize,
    horizontal_offset: usize,
}

#[derive(Clone, Debug)]
struct CachedFrame {
    signature: FrameSignature,
    frame: RenderFrame,
}

fn frame_signature(
    app: &BlitzApp,
    state: &NotepadUiState,
    width: usize,
    height: usize,
    gui_state: &GuiState,
) -> FrameSignature {
    FrameSignature {
        width,
        height,
        ui_state: state.clone(),
        document_line_count: app.document().line_count_snapshot(),
        active_menu: gui_state.active_menu,
        dialog: gui_state.dialog.clone(),
        first_visible_line: gui_state.first_visible_line,
        horizontal_offset: gui_state.horizontal_offset,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SearchSpec {
    query: String,
    match_case: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DialogState {
    Find {
        query: String,
        match_case: bool,
        forward: bool,
    },
    Replace {
        query: String,
        replacement: String,
        active_field: ReplaceField,
        match_case: bool,
    },
    GoTo {
        line: String,
    },
    Info {
        title: String,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplaceField {
    Find,
    Replace,
}

struct TextInput {
    queue: Rc<RefCell<Vec<char>>>,
}

impl TextInput {
    fn new(queue: Rc<RefCell<Vec<char>>>) -> Self {
        Self { queue }
    }
}

impl InputCallback for TextInput {
    fn add_char(&mut self, uni_char: u32) {
        if let Some(character) = char::from_u32(uni_char) {
            self.queue.borrow_mut().push(character);
        }
    }
}

fn handle_mouse(window: &Window, app: &mut BlitzApp, gui_state: &mut GuiState) -> Result<bool> {
    let mouse_down = window.get_mouse_down(MouseButton::Left);
    let clicked = mouse_down && !gui_state.mouse_was_down;
    gui_state.mouse_was_down = mouse_down;
    if !mouse_down {
        gui_state.scroll_drag = None;
        return Ok(false);
    }

    let Some((mouse_x, mouse_y)) = window.get_mouse_pos(MouseMode::Discard) else {
        return Ok(false);
    };
    let (width, height) = window.get_size();
    let width = width.max(MIN_WIDTH);
    let height = height.max(MIN_HEIGHT);
    let x = mouse_x as usize;
    let y = mouse_y as usize;

    if gui_state.scroll_drag.is_some() {
        update_scroll_drag(app, gui_state, width, height, x, y)?;
        return Ok(false);
    }

    if !clicked {
        return Ok(false);
    }

    if gui_state.dialog.is_some() {
        return Ok(false);
    }

    if let Some(menu_index) = menu_title_hit(&gui_state.fonts, x, y) {
        gui_state.active_menu = if gui_state.active_menu == Some(menu_index) {
            None
        } else {
            Some(menu_index)
        };
        return Ok(false);
    }

    if let Some(menu_index) = gui_state.active_menu {
        if let Some(row) = menu_row_hit(app, &gui_state.fonts, menu_index, x, y) {
            gui_state.active_menu = None;
            return execute_menu_row(app, gui_state, menu_index, row);
        }
        gui_state.active_menu = None;
    }

    if handle_scrollbar_click(app, gui_state, width, height, x, y)? {
        return Ok(false);
    }

    if let Some(offset) = text_offset_for_point(app, gui_state, width, height, x, y) {
        app.set_caret_offset(offset)?;
    }
    Ok(false)
}

fn handle_scrollbar_click(
    app: &BlitzApp,
    gui_state: &mut GuiState,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
) -> Result<bool> {
    let state = app.ui_state()?;
    let Some(metrics) = editor_metrics(&state, width, height) else {
        return Ok(false);
    };

    if x >= metrics.text_width && y >= metrics.text_top && y < metrics.editor_bottom {
        let (thumb_y, thumb_height) = scroll_thumb(
            metrics.text_top,
            metrics.text_height,
            app.document().line_count(),
            metrics.visible_line_count,
            gui_state.first_visible_line,
        );
        if y >= thumb_y && y < thumb_y + thumb_height {
            gui_state.scroll_drag = Some(ScrollDrag::Vertical {
                grab_offset: y - thumb_y,
            });
            return Ok(true);
        }
        let page = metrics.visible_line_count as isize;
        let delta = if y < thumb_y {
            -page
        } else if y >= thumb_y + thumb_height {
            page
        } else {
            0
        };
        gui_state.scroll_vertical(app, delta, metrics.visible_line_count);
        return Ok(true);
    }

    if let Some(scroll_y) = metrics.horizontal_scroll_y {
        if y >= scroll_y && y < scroll_y + SCROLLBAR_WIDTH && x < metrics.text_width {
            let max_line_len = max_visible_line_len(
                app,
                gui_state.first_visible_line,
                metrics.visible_line_count,
            );
            let visible_bytes = visible_byte_capacity(&gui_state.fonts, metrics.editor_text_width);
            let (thumb_x, thumb_width) = scroll_thumb(
                0,
                metrics.text_width,
                max_line_len.max(visible_bytes),
                visible_bytes,
                gui_state.horizontal_offset,
            );
            if x >= thumb_x && x < thumb_x + thumb_width {
                gui_state.scroll_drag = Some(ScrollDrag::Horizontal {
                    grab_offset: x - thumb_x,
                });
                return Ok(true);
            }
            let page = visible_bytes as isize;
            let delta = if x < thumb_x {
                -page
            } else if x >= thumb_x + thumb_width {
                page
            } else {
                0
            };
            gui_state.scroll_horizontal(app, delta, metrics.visible_line_count);
            return Ok(true);
        }
    }

    Ok(false)
}

fn update_scroll_drag(
    app: &BlitzApp,
    gui_state: &mut GuiState,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
) -> Result<()> {
    let state = app.ui_state()?;
    let Some(metrics) = editor_metrics(&state, width, height) else {
        return Ok(());
    };

    match gui_state.scroll_drag {
        Some(ScrollDrag::Vertical { grab_offset }) => {
            let (_, thumb_height) = scroll_thumb(
                metrics.text_top,
                metrics.text_height,
                app.document().line_count(),
                metrics.visible_line_count,
                gui_state.first_visible_line,
            );
            let thumb_y = y.saturating_sub(grab_offset).clamp(
                metrics.text_top,
                metrics.editor_bottom.saturating_sub(thumb_height),
            );
            gui_state.first_visible_line = offset_for_thumb(
                metrics.text_top,
                metrics.text_height,
                thumb_height,
                app.document().line_count(),
                metrics.visible_line_count,
                thumb_y,
            );
            gui_state.scroll_vertical(app, 0, metrics.visible_line_count);
        }
        Some(ScrollDrag::Horizontal { grab_offset }) => {
            let max_line_len = max_visible_line_len(
                app,
                gui_state.first_visible_line,
                metrics.visible_line_count,
            );
            let visible_bytes = visible_byte_capacity(&gui_state.fonts, metrics.editor_text_width);
            let (_, thumb_width) = scroll_thumb(
                0,
                metrics.text_width,
                max_line_len.max(visible_bytes),
                visible_bytes,
                gui_state.horizontal_offset,
            );
            let thumb_x = x
                .saturating_sub(grab_offset)
                .clamp(0, metrics.text_width.saturating_sub(thumb_width));
            gui_state.horizontal_offset = offset_for_thumb(
                0,
                metrics.text_width,
                thumb_width,
                max_line_len.max(visible_bytes),
                visible_bytes,
                thumb_x,
            );
            gui_state.clamp_horizontal(app, metrics.visible_line_count);
        }
        None => {}
    }

    Ok(())
}

fn handle_scroll_wheel(
    window: &Window,
    app: &BlitzApp,
    gui_state: &mut GuiState,
    width: usize,
    height: usize,
) -> Result<()> {
    let Some((scroll_x, scroll_y)) = window.get_scroll_wheel() else {
        return Ok(());
    };
    let state = app.ui_state()?;
    let Some(metrics) = editor_metrics(&state, width, height) else {
        return Ok(());
    };

    if !is_shift_down(window) {
        let delta_lines = (-(scroll_y * WHEEL_LINES as f32).round()) as isize;
        if delta_lines != 0 {
            gui_state.scroll_vertical(app, delta_lines, metrics.visible_line_count);
        }
    }

    if metrics.horizontal_scroll_y.is_some() {
        let horizontal_scroll = scroll_x + if is_shift_down(window) { scroll_y } else { 0.0 };
        let delta_bytes = (-(horizontal_scroll * HORIZONTAL_WHEEL_BYTES as f32).round()) as isize;
        if delta_bytes != 0 {
            gui_state.scroll_horizontal(app, delta_bytes, metrics.visible_line_count);
        }
    }

    Ok(())
}

fn scroll_page(
    window: &Window,
    app: &BlitzApp,
    gui_state: &mut GuiState,
    direction: isize,
) -> Result<()> {
    let (width, height) = window.get_size();
    let state = app.ui_state()?;
    if let Some(metrics) = editor_metrics(&state, width.max(MIN_WIDTH), height.max(MIN_HEIGHT)) {
        gui_state.scroll_vertical(
            app,
            direction * metrics.visible_line_count as isize,
            metrics.visible_line_count,
        );
    }
    Ok(())
}

fn handle_keys(window: &Window, app: &mut BlitzApp, gui_state: &mut GuiState) -> Result<()> {
    if gui_state.dialog.is_some() {
        return handle_dialog_keys(window, app, gui_state);
    }

    if window.is_key_pressed(Key::Escape, KeyRepeat::No) {
        gui_state.active_menu = None;
    }

    if is_command_down(window) {
        if window.is_key_pressed(Key::N, KeyRepeat::No) {
            if is_shift_down(window) {
                open_new_window(gui_state);
            } else if confirm_unsaved_changes(app, gui_state)? {
                app.new_document();
                gui_state.reset_scroll(app);
                gui_state.set_message("New document");
            }
        }
        if window.is_key_pressed(Key::O, KeyRepeat::No) {
            open_document_dialog(app, gui_state)?;
        }
        if window.is_key_pressed(Key::S, KeyRepeat::No) {
            if is_shift_down(window) {
                save_as_dialog(app, gui_state)?;
            } else {
                save_document_or_dialog(app, gui_state)?;
            }
        }
        if window.is_key_pressed(Key::P, KeyRepeat::No) {
            gui_state.dialog = Some(DialogState::Info {
                title: "Print".to_owned(),
                message: "Native print dialog integration is not available in this software-rendered window yet.".to_owned(),
            });
        }
        if window.is_key_pressed(Key::F, KeyRepeat::No) {
            gui_state.dialog = Some(DialogState::Find {
                query: app
                    .selected_text()
                    .or_else(|| {
                        gui_state
                            .last_search
                            .as_ref()
                            .map(|search| search.query.clone())
                    })
                    .unwrap_or_default(),
                match_case: false,
                forward: true,
            });
        }
        if window.is_key_pressed(Key::H, KeyRepeat::No) {
            gui_state.dialog = Some(DialogState::Replace {
                query: app
                    .selected_text()
                    .or_else(|| {
                        gui_state
                            .last_search
                            .as_ref()
                            .map(|search| search.query.clone())
                    })
                    .unwrap_or_default(),
                replacement: String::new(),
                active_field: ReplaceField::Find,
                match_case: false,
            });
        }
        if window.is_key_pressed(Key::G, KeyRepeat::No) && !app.ui_state()?.word_wrap {
            gui_state.dialog = Some(DialogState::GoTo {
                line: String::new(),
            });
        }
        if window.is_key_pressed(Key::E, KeyRepeat::No) {
            search_with_bing(app, gui_state);
        }
        if window.is_key_pressed(Key::A, KeyRepeat::No) {
            app.select_all();
        }
        if window.is_key_pressed(Key::X, KeyRepeat::No) {
            if let Some(text) = app.cut_selection()? {
                set_clipboard_text(gui_state, text);
                gui_state.set_message("Cut");
            }
        }
        if window.is_key_pressed(Key::C, KeyRepeat::No) {
            if let Some(text) = app.selected_text() {
                set_clipboard_text(gui_state, text);
                gui_state.set_message("Copied");
            }
        }
        if window.is_key_pressed(Key::V, KeyRepeat::No) {
            paste_from_clipboard(app, gui_state)?;
        }
        if window.is_key_pressed(Key::Z, KeyRepeat::No) {
            app.undo()?;
        }
        if window.is_key_pressed(Key::Key0, KeyRepeat::No)
            || window.is_key_pressed(Key::NumPad0, KeyRepeat::No)
        {
            app.restore_default_zoom();
        }
        if window.is_key_pressed(Key::Equal, KeyRepeat::Yes)
            || window.is_key_pressed(Key::NumPadPlus, KeyRepeat::Yes)
        {
            app.zoom_in();
        }
        if window.is_key_pressed(Key::Minus, KeyRepeat::Yes)
            || window.is_key_pressed(Key::NumPadMinus, KeyRepeat::Yes)
        {
            app.zoom_out();
        }
        return Ok(());
    }

    for key in window.get_keys_pressed(KeyRepeat::Yes) {
        match key {
            Key::Left => app.move_left()?,
            Key::Right => app.move_right()?,
            Key::Up => app.move_up()?,
            Key::Down => app.move_down()?,
            Key::PageUp => scroll_page(window, app, gui_state, -1)?,
            Key::PageDown => scroll_page(window, app, gui_state, 1)?,
            Key::Home => app.move_line_start()?,
            Key::End => app.move_line_end()?,
            Key::Backspace => {
                app.backspace()?;
            }
            Key::Delete => {
                app.delete_forward()?;
            }
            Key::Enter | Key::NumPadEnter => app.insert_text("\n")?,
            Key::Tab => app.insert_text("\t")?,
            Key::F5 => app.insert_time_date()?,
            _ => {}
        }
    }

    if window.is_key_pressed(Key::F3, KeyRepeat::No) {
        find_again(app, gui_state, !is_shift_down(window))?;
    }
    Ok(())
}

fn handle_text_input(
    input_queue: &Rc<RefCell<Vec<char>>>,
    window: &Window,
    app: &mut BlitzApp,
    gui_state: &mut GuiState,
) -> Result<()> {
    if gui_state.dialog.is_some() {
        let characters = input_queue.borrow_mut().drain(..).collect::<Vec<_>>();
        for character in characters {
            if !character.is_control() {
                append_dialog_character(gui_state, character);
            }
        }
        return Ok(());
    }

    if is_command_down(window) {
        input_queue.borrow_mut().clear();
        return Ok(());
    }

    let characters = input_queue.borrow_mut().drain(..).collect::<Vec<_>>();
    for character in characters {
        if !character.is_control() {
            app.insert_text(&character.to_string())?;
        }
    }
    Ok(())
}

fn handle_dialog_keys(window: &Window, app: &mut BlitzApp, gui_state: &mut GuiState) -> Result<()> {
    if window.is_key_pressed(Key::Escape, KeyRepeat::No) {
        gui_state.dialog = None;
        return Ok(());
    }

    if window.is_key_pressed(Key::Backspace, KeyRepeat::Yes) {
        backspace_dialog_field(gui_state);
    }
    if window.is_key_pressed(Key::Tab, KeyRepeat::No) {
        tab_dialog_field(gui_state);
    }
    if window.is_key_pressed(Key::Space, KeyRepeat::No) {
        toggle_dialog_option(gui_state);
    }
    if window.is_key_pressed(Key::Enter, KeyRepeat::No)
        || window.is_key_pressed(Key::NumPadEnter, KeyRepeat::No)
    {
        accept_dialog(app, gui_state, is_command_down(window))?;
    }
    Ok(())
}

fn append_dialog_character(gui_state: &mut GuiState, character: char) {
    match gui_state.dialog.as_mut() {
        Some(DialogState::Find { query, .. }) => query.push(character),
        Some(DialogState::Replace {
            query,
            replacement,
            active_field,
            ..
        }) => match active_field {
            ReplaceField::Find => query.push(character),
            ReplaceField::Replace => replacement.push(character),
        },
        Some(DialogState::GoTo { line }) if character.is_ascii_digit() => line.push(character),
        _ => {}
    }
}

fn backspace_dialog_field(gui_state: &mut GuiState) {
    match gui_state.dialog.as_mut() {
        Some(DialogState::Find { query, .. }) => {
            query.pop();
        }
        Some(DialogState::Replace {
            query,
            replacement,
            active_field,
            ..
        }) => match active_field {
            ReplaceField::Find => {
                query.pop();
            }
            ReplaceField::Replace => {
                replacement.pop();
            }
        },
        Some(DialogState::GoTo { line }) => {
            line.pop();
        }
        _ => {}
    }
}

fn tab_dialog_field(gui_state: &mut GuiState) {
    if let Some(DialogState::Replace { active_field, .. }) = gui_state.dialog.as_mut() {
        *active_field = match active_field {
            ReplaceField::Find => ReplaceField::Replace,
            ReplaceField::Replace => ReplaceField::Find,
        };
    }
}

fn toggle_dialog_option(gui_state: &mut GuiState) {
    match gui_state.dialog.as_mut() {
        Some(DialogState::Find {
            match_case,
            forward,
            ..
        }) => {
            if *match_case {
                *match_case = false;
                *forward = !*forward;
            } else {
                *match_case = true;
            }
        }
        Some(DialogState::Replace { match_case, .. }) => *match_case = !*match_case,
        _ => {}
    }
}

fn accept_dialog(app: &mut BlitzApp, gui_state: &mut GuiState, command_down: bool) -> Result<()> {
    let Some(dialog) = gui_state.dialog.clone() else {
        return Ok(());
    };

    match dialog {
        DialogState::Find {
            query,
            match_case,
            forward,
        } => {
            if app.find_text(&query, match_case, forward)? {
                gui_state.last_search = Some(SearchSpec { query, match_case });
                gui_state.set_message("Found");
            } else {
                gui_state.set_message("Cannot find text");
            }
        }
        DialogState::Replace {
            query,
            replacement,
            match_case,
            ..
        } => {
            gui_state.last_search = Some(SearchSpec {
                query: query.clone(),
                match_case,
            });
            if command_down {
                let count = app.replace_all(&query, &replacement, match_case)?;
                gui_state.set_message(format!("Replaced {count}"));
            } else if app.replace_next(&query, &replacement, match_case)? {
                gui_state.set_message("Replaced");
            } else {
                gui_state.set_message("Cannot find text");
            }
        }
        DialogState::GoTo { line } => {
            let parsed = line.parse::<usize>().unwrap_or(0);
            if app.go_to_line(parsed)? {
                gui_state.dialog = None;
                gui_state.set_message(format!("Ln {parsed}"));
            } else {
                gui_state.set_message("Invalid line number");
            }
        }
        DialogState::Info { .. } => {
            gui_state.dialog = None;
        }
    }
    Ok(())
}

fn is_command_down(window: &Window) -> bool {
    window.is_key_down(Key::LeftCtrl)
        || window.is_key_down(Key::RightCtrl)
        || window.is_key_down(Key::LeftSuper)
        || window.is_key_down(Key::RightSuper)
}

fn is_shift_down(window: &Window) -> bool {
    window.is_key_down(Key::LeftShift) || window.is_key_down(Key::RightShift)
}

fn execute_menu_row(
    app: &mut BlitzApp,
    gui_state: &mut GuiState,
    menu_index: usize,
    row: MenuRow,
) -> Result<bool> {
    if !row.enabled || row.has_children {
        return Ok(false);
    }

    match (MENU_BAR[menu_index], row.label.as_str()) {
        ("File", "New") => {
            if confirm_unsaved_changes(app, gui_state)? {
                app.new_document();
                gui_state.reset_scroll(app);
                gui_state.set_message("New document");
            }
        }
        ("File", "New Window") => open_new_window(gui_state),
        ("File", "Open...") => open_document_dialog(app, gui_state)?,
        ("File", "Save") => {
            save_document_or_dialog(app, gui_state)?;
        }
        ("File", "Save As...") => save_as_dialog(app, gui_state)?,
        ("File", "Page Setup...") => {
            gui_state.dialog = Some(DialogState::Info {
                title: "Page Setup".to_owned(),
                message: "Page setup options are not persisted yet. Header, footer, margins, and orientation will be added to the print pipeline.".to_owned(),
            });
        }
        ("File", "Print...") => {
            gui_state.dialog = Some(DialogState::Info {
                title: "Print".to_owned(),
                message: "Native print dialog integration is not available in this software-rendered window yet.".to_owned(),
            });
        }
        ("File", "Exit") => return confirm_unsaved_changes(app, gui_state),
        ("Edit", "Undo") => {
            app.undo()?;
        }
        ("Edit", "Cut") => {
            if let Some(text) = app.cut_selection()? {
                set_clipboard_text(gui_state, text);
                gui_state.set_message("Cut");
            }
        }
        ("Edit", "Copy") => {
            if let Some(text) = app.selected_text() {
                set_clipboard_text(gui_state, text);
                gui_state.set_message("Copied");
            }
        }
        ("Edit", "Paste") => {
            paste_from_clipboard(app, gui_state)?;
        }
        ("Edit", "Delete") => {
            app.delete_forward()?;
        }
        ("Edit", "Select All") => app.select_all(),
        ("Edit", "Time/Date") => app.insert_time_date()?,
        ("Edit", "Search with Bing...") => search_with_bing(app, gui_state),
        ("Edit", "Find...") => {
            gui_state.dialog = Some(DialogState::Find {
                query: app
                    .selected_text()
                    .or_else(|| {
                        gui_state
                            .last_search
                            .as_ref()
                            .map(|search| search.query.clone())
                    })
                    .unwrap_or_default(),
                match_case: false,
                forward: true,
            });
        }
        ("Edit", "Find Next") => find_again(app, gui_state, true)?,
        ("Edit", "Find Previous") => find_again(app, gui_state, false)?,
        ("Edit", "Replace...") => {
            gui_state.dialog = Some(DialogState::Replace {
                query: app
                    .selected_text()
                    .or_else(|| {
                        gui_state
                            .last_search
                            .as_ref()
                            .map(|search| search.query.clone())
                    })
                    .unwrap_or_default(),
                replacement: String::new(),
                active_field: ReplaceField::Find,
                match_case: false,
            });
        }
        ("Edit", "Go To...") => {
            gui_state.dialog = Some(DialogState::GoTo {
                line: String::new(),
            });
        }
        ("Format", "Word Wrap") => app.toggle_word_wrap(),
        ("Format", "Font...") => {
            gui_state.dialog = Some(DialogState::Info {
                title: "Font".to_owned(),
                message: format!("Current font stack: {}\nSample: AaBbYyZz\nAptos / Yu Gothic UI are preferred when available.", gui_state.fonts.description()),
            });
        }
        ("View", "Zoom In") => app.zoom_in(),
        ("View", "Zoom Out") => app.zoom_out(),
        ("View", "Restore Default Zoom") => app.restore_default_zoom(),
        ("View", "Status Bar") => app.toggle_status_bar(),
        ("Help", "View Help") => open_browser("https://github.com/", gui_state),
        ("Help", "Send Feedback") => open_browser("https://github.com/", gui_state),
        ("Help", "About Notepad") => {
            gui_state.dialog = Some(DialogState::Info {
                title: "About Notepad".to_owned(),
                message: "blitzpad 0.1.0\nNotepad-compatible editor".to_owned(),
            });
        }
        (_, label) => gui_state.set_message(format!("{label} is not available yet")),
    }
    Ok(false)
}

fn find_again(app: &mut BlitzApp, gui_state: &mut GuiState, forward: bool) -> Result<()> {
    let Some(search) = gui_state.last_search.clone() else {
        gui_state.set_message("No active search");
        return Ok(());
    };
    if app.find_text(&search.query, search.match_case, forward)? {
        gui_state.set_message("Found");
    } else {
        gui_state.set_message("Cannot find text");
    }
    Ok(())
}

fn open_document_dialog(app: &mut BlitzApp, gui_state: &mut GuiState) -> Result<()> {
    if !confirm_unsaved_changes(app, gui_state)? {
        return Ok(());
    }
    let Some(path) = FileDialog::new()
        .add_filter("Text", &["txt", "log", "md", "rs", "toml"])
        .pick_file()
    else {
        return Ok(());
    };
    app.open_document(&path)?;
    gui_state.reset_scroll(app);
    gui_state.set_message(format!("Opened {}", path.display()));
    Ok(())
}

fn confirm_unsaved_changes(app: &mut BlitzApp, gui_state: &mut GuiState) -> Result<bool> {
    if gui_state.save_job.is_some() {
        gui_state.set_message("Saving");
        return Ok(false);
    }

    if !app.document().is_dirty() {
        return Ok(true);
    }

    if cfg!(test) {
        return Ok(true);
    }

    let file_name = app.document().file_name();
    let result = MessageDialog::new()
        .set_level(MessageLevel::Warning)
        .set_title("Notepad")
        .set_description(format!("Do you want to save changes to {file_name}?"))
        .set_buttons(MessageButtons::YesNoCancel)
        .show();

    match result {
        MessageDialogResult::Yes => {
            save_document_or_dialog_blocking(app, gui_state)?;
            Ok(!app.document().is_dirty())
        }
        MessageDialogResult::No => Ok(true),
        _ => Ok(false),
    }
}

fn save_document_or_dialog(app: &mut BlitzApp, gui_state: &mut GuiState) -> Result<()> {
    if start_background_save(app, gui_state)? {
        gui_state.set_message("Saving");
    } else {
        save_as_dialog(app, gui_state)?;
    }
    Ok(())
}

fn save_document_or_dialog_blocking(app: &mut BlitzApp, gui_state: &mut GuiState) -> Result<()> {
    if app.save()? {
        gui_state.set_message("Saved");
    } else {
        save_as_dialog(app, gui_state)?;
    }
    Ok(())
}

fn start_background_save(app: &BlitzApp, gui_state: &mut GuiState) -> Result<bool> {
    if gui_state.save_job.is_some() {
        gui_state.set_message("Saving");
        return Ok(true);
    }

    let Some(snapshot) = app.save_snapshot() else {
        return Ok(false);
    };
    let generation = snapshot.generation;
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("blitz-save".to_owned())
        .spawn(move || {
            let result = snapshot.document.save_snapshot_to_path(
                &snapshot.path,
                snapshot.encoding,
                snapshot.line_ending,
            );
            let _ = sender.send(SaveJobResult { generation, result });
        })?;
    gui_state.save_job = Some(SaveJob { receiver });
    Ok(true)
}

fn poll_save_job(app: &mut BlitzApp, gui_state: &mut GuiState) {
    let Some(received) = gui_state
        .save_job
        .as_ref()
        .map(|job| job.receiver.try_recv())
    else {
        return;
    };

    match received {
        Ok(result) => {
            gui_state.save_job = None;
            match result.result {
                Ok(saved_len) => {
                    if app.complete_save_snapshot(result.generation, saved_len) {
                        gui_state.set_message("Saved");
                    } else {
                        gui_state.set_message("Saved snapshot; newer edits remain");
                    }
                }
                Err(error) => {
                    gui_state.dialog = Some(DialogState::Info {
                        title: "Save".to_owned(),
                        message: format!("Save failed: {error}"),
                    });
                }
            }
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => {
            gui_state.save_job = None;
            gui_state.dialog = Some(DialogState::Info {
                title: "Save".to_owned(),
                message: "Save failed: worker stopped".to_owned(),
            });
        }
    }
}

fn save_as_dialog(app: &mut BlitzApp, gui_state: &mut GuiState) -> Result<()> {
    if gui_state.save_job.is_some() {
        gui_state.set_message("Saving");
        return Ok(());
    }

    let Some(path) = FileDialog::new()
        .add_filter("Text", &["txt", "log", "md"])
        .set_file_name("Untitled.txt")
        .save_file()
    else {
        return Ok(());
    };
    app.save_as_path(&path)?;
    gui_state.set_message(format!("Saved {}", path.display()));
    Ok(())
}

fn open_new_window(gui_state: &mut GuiState) {
    match env::current_exe().and_then(|path| Command::new(path).spawn().map(|_| ())) {
        Ok(()) => gui_state.set_message("Opened new window"),
        Err(error) => gui_state.set_message(format!("New Window failed: {error}")),
    }
}

fn set_clipboard_text(gui_state: &mut GuiState, text: String) {
    if let Ok(mut clipboard) = Clipboard::new() {
        let _ = clipboard.set_text(text.clone());
    }
    gui_state.clipboard = text;
}

fn paste_from_clipboard(app: &mut BlitzApp, gui_state: &mut GuiState) -> Result<()> {
    let clipboard_text = Clipboard::new()
        .ok()
        .and_then(|mut clipboard| clipboard.get_text().ok())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| gui_state.clipboard.clone());

    if !clipboard_text.is_empty() {
        app.paste_text(&clipboard_text)?;
        gui_state.clipboard = clipboard_text;
    }
    Ok(())
}

fn search_with_bing(app: &BlitzApp, gui_state: &mut GuiState) {
    let Some(query) = app.selected_text().filter(|text| !text.is_empty()) else {
        gui_state.set_message("Select text before searching");
        return;
    };
    let url = format!(
        "https://www.bing.com/search?q={}",
        urlencoding::encode(&query)
    );
    open_browser(&url, gui_state);
}

fn open_browser(url: &str, gui_state: &mut GuiState) {
    match webbrowser::open(url) {
        Ok(_) => gui_state.set_message("Opened browser"),
        Err(error) => gui_state.set_message(format!("Browser open failed: {error}")),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderFrame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
}

pub fn render_frame(app: &BlitzApp, width: usize, height: usize) -> Result<RenderFrame> {
    let gui_state = GuiState::new()?;
    app.document().refresh_line_index();
    let state = app.ui_state()?;
    render_frame_with_state(app, &state, width, height, &gui_state)
}

pub fn save_startup_screenshot(app: &BlitzApp, path: impl AsRef<Path>) -> Result<()> {
    let frame = render_frame(app, DEFAULT_WIDTH, DEFAULT_HEIGHT)?;
    write_png(&frame, path)
}

pub fn write_png(frame: &RenderFrame, path: impl AsRef<Path>) -> Result<()> {
    let file = File::create(path.as_ref()).map_err(|error| BlitzError::Io {
        path: path.as_ref().to_path_buf(),
        source: error,
    })?;
    let writer = BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, frame.width as u32, frame.height as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut png_writer = encoder
        .write_header()
        .map_err(|error| BlitzError::Window(error.to_string()))?;

    let mut bytes = Vec::with_capacity(frame.width * frame.height * 3);
    for pixel in &frame.pixels {
        bytes.push(((pixel >> 16) & 0xff) as u8);
        bytes.push(((pixel >> 8) & 0xff) as u8);
        bytes.push((pixel & 0xff) as u8);
    }
    png_writer
        .write_image_data(&bytes)
        .map_err(|error| BlitzError::Window(error.to_string()))
}

fn render_frame_with_state(
    app: &BlitzApp,
    state: &NotepadUiState,
    width: usize,
    height: usize,
    gui_state: &GuiState,
) -> Result<RenderFrame> {
    let mut canvas = Canvas::new(width.max(1), height.max(1), COLOR_WINDOW);
    draw_chrome(&mut canvas, state, gui_state);
    draw_text_area(&mut canvas, app, state, gui_state);
    if let Some(menu_index) = gui_state.active_menu {
        draw_menu_popup(&mut canvas, app, menu_index, &gui_state.fonts);
    }
    if let Some(dialog) = &gui_state.dialog {
        draw_dialog(&mut canvas, dialog, &gui_state.fonts);
    }
    Ok(canvas.into_frame())
}

fn draw_chrome(canvas: &mut Canvas, state: &NotepadUiState, gui_state: &GuiState) {
    let menu_top = menu_top();
    canvas.fill_rect(0, menu_top, canvas.width, MENU_HEIGHT, COLOR_WINDOW);
    canvas.line(
        0,
        menu_top + MENU_HEIGHT,
        canvas.width,
        menu_top + MENU_HEIGHT,
        COLOR_BORDER,
    );

    for title in menu_titles(&gui_state.fonts) {
        if gui_state.active_menu == Some(title.index) {
            canvas.fill_rect(
                title.x,
                menu_top + 1,
                title.width,
                MENU_HEIGHT - 2,
                COLOR_MENU_OPEN,
            );
        }
        canvas.text(
            title.x + 8,
            menu_top + 7,
            title.label,
            COLOR_TEXT,
            TextRole::Ui,
            &gui_state.fonts,
        );
    }

    if state.status_bar_visible && canvas.height > STATUS_HEIGHT {
        let status_y = canvas.height - STATUS_HEIGHT;
        canvas.fill_rect(0, status_y, canvas.width, STATUS_HEIGHT, COLOR_STATUS);
        canvas.line(0, status_y, canvas.width, status_y, COLOR_BORDER);

        let mut x = 10usize;
        for cell in state.status_cells() {
            if x > 10 {
                canvas.line(
                    x.saturating_sub(12),
                    status_y,
                    x.saturating_sub(12),
                    canvas.height,
                    COLOR_BORDER,
                );
            }
            canvas.text(
                x,
                status_y + 7,
                &cell,
                COLOR_TEXT,
                TextRole::Ui,
                &gui_state.fonts,
            );
            x += gui_state.fonts.measure(&cell, TextRole::Ui) + 40;
            if x >= canvas.width {
                break;
            }
        }
    }
}

fn draw_menu_popup(canvas: &mut Canvas, app: &BlitzApp, menu_index: usize, fonts: &FontStack) {
    let Some(title) = menu_titles(fonts)
        .into_iter()
        .find(|title| title.index == menu_index)
    else {
        return;
    };
    let rows = menu_rows(app, menu_index);
    let metrics = menu_metrics(fonts, &rows);
    let x = title.x;
    let y = dropdown_top();
    canvas.fill_rect(x + 3, y + 3, metrics.width, metrics.height, 0x00888888);
    canvas.fill_rect(x, y, metrics.width, metrics.height, COLOR_DROPDOWN);
    canvas.rect(x, y, metrics.width, metrics.height, COLOR_BORDER);

    let mut row_y = y + 4;
    for row in rows {
        if row.separator_before {
            canvas.line(
                x + 6,
                row_y + 3,
                x + metrics.width - 6,
                row_y + 3,
                COLOR_BORDER,
            );
            row_y += MENU_SEPARATOR_HEIGHT;
        }
        let color = if row.enabled {
            COLOR_TEXT
        } else {
            COLOR_DISABLED_TEXT
        };
        if row.checked {
            canvas.text(x + 9, row_y + 7, "*", COLOR_TEXT, TextRole::Ui, fonts);
        }
        canvas.text(
            x + 28 + row.indent,
            row_y + 7,
            &row.label,
            color,
            TextRole::Ui,
            fonts,
        );
        if let Some(shortcut) = &row.shortcut {
            canvas.text(
                x + metrics.shortcut_x,
                row_y + 7,
                shortcut,
                color,
                TextRole::Ui,
                fonts,
            );
        }
        if row.has_children {
            canvas.text(
                x + metrics.width - 18,
                row_y + 7,
                ">",
                color,
                TextRole::Ui,
                fonts,
            );
        }
        row_y += MENU_ITEM_HEIGHT;
    }
}

fn draw_dialog(canvas: &mut Canvas, dialog: &DialogState, fonts: &FontStack) {
    let width = 430usize.min(canvas.width.saturating_sub(40));
    let height = match dialog {
        DialogState::Replace { .. } => 180,
        DialogState::Info { .. } => 160,
        _ => 140,
    };
    let x = (canvas.width.saturating_sub(width)) / 2;
    let y = (canvas.height.saturating_sub(height)) / 2;

    canvas.fill_rect(x + 4, y + 4, width, height, 0x00909090);
    canvas.fill_rect(x, y, width, height, COLOR_DROPDOWN);
    canvas.rect(x, y, width, height, COLOR_BORDER);

    match dialog {
        DialogState::Find {
            query,
            match_case,
            forward,
        } => {
            canvas.text(x + 16, y + 12, "Find", COLOR_TEXT, TextRole::Ui, fonts);
            draw_text_field(canvas, fonts, x + 16, y + 44, width - 32, query, true);
            canvas.text(
                x + 16,
                y + 82,
                &format!(
                    "[Space] Match case: {}   Direction: {}",
                    on_off(*match_case),
                    if *forward { "Down" } else { "Up" }
                ),
                COLOR_TEXT,
                TextRole::Ui,
                fonts,
            );
            canvas.text(
                x + 16,
                y + 112,
                "Enter: Find   Esc: Close",
                COLOR_TEXT,
                TextRole::Ui,
                fonts,
            );
        }
        DialogState::Replace {
            query,
            replacement,
            active_field,
            match_case,
        } => {
            canvas.text(x + 16, y + 12, "Replace", COLOR_TEXT, TextRole::Ui, fonts);
            canvas.text(
                x + 16,
                y + 42,
                "Find what:",
                COLOR_TEXT,
                TextRole::Ui,
                fonts,
            );
            draw_text_field(
                canvas,
                fonts,
                x + 112,
                y + 36,
                width - 128,
                query,
                *active_field == ReplaceField::Find,
            );
            canvas.text(
                x + 16,
                y + 78,
                "Replace with:",
                COLOR_TEXT,
                TextRole::Ui,
                fonts,
            );
            draw_text_field(
                canvas,
                fonts,
                x + 112,
                y + 72,
                width - 128,
                replacement,
                *active_field == ReplaceField::Replace,
            );
            canvas.text(
                x + 16,
                y + 114,
                &format!("[Tab] Field   [Space] Match case: {}", on_off(*match_case)),
                COLOR_TEXT,
                TextRole::Ui,
                fonts,
            );
            canvas.text(
                x + 16,
                y + 144,
                "Enter: Replace Next   Ctrl/Cmd+Enter: Replace All",
                COLOR_TEXT,
                TextRole::Ui,
                fonts,
            );
        }
        DialogState::GoTo { line } => {
            canvas.text(
                x + 16,
                y + 12,
                "Go To Line",
                COLOR_TEXT,
                TextRole::Ui,
                fonts,
            );
            draw_text_field(canvas, fonts, x + 16, y + 44, width - 32, line, true);
            canvas.text(
                x + 16,
                y + 86,
                "Enter: Go To   Esc: Close",
                COLOR_TEXT,
                TextRole::Ui,
                fonts,
            );
        }
        DialogState::Info { title, message } => {
            canvas.text(x + 16, y + 12, title, COLOR_TEXT, TextRole::Ui, fonts);
            for (index, line) in message.lines().take(4).enumerate() {
                canvas.text(
                    x + 16,
                    y + 48 + index * 24,
                    line,
                    COLOR_TEXT,
                    TextRole::Ui,
                    fonts,
                );
            }
            canvas.text(
                x + 16,
                y + height - 34,
                "Enter/Esc: Close",
                COLOR_TEXT,
                TextRole::Ui,
                fonts,
            );
        }
    }
}

fn draw_text_field(
    canvas: &mut Canvas,
    fonts: &FontStack,
    x: usize,
    y: usize,
    width: usize,
    text: &str,
    active: bool,
) {
    canvas.fill_rect(x, y, width, 28, COLOR_TEXT_AREA);
    canvas.rect(
        x,
        y,
        width,
        28,
        if active { COLOR_TEXT } else { COLOR_BORDER },
    );
    canvas.text(x + 6, y + 6, text, COLOR_TEXT, TextRole::Ui, fonts);
}

fn on_off(value: bool) -> &'static str {
    if value {
        "On"
    } else {
        "Off"
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EditorMetrics {
    text_top: usize,
    text_bottom: usize,
    editor_bottom: usize,
    text_width: usize,
    text_height: usize,
    editor_text_width: usize,
    visible_line_count: usize,
    horizontal_scroll_y: Option<usize>,
}

fn editor_metrics(state: &NotepadUiState, width: usize, height: usize) -> Option<EditorMetrics> {
    let status_height = if state.status_bar_visible {
        STATUS_HEIGHT
    } else {
        0
    };
    let text_top = text_top();
    let text_bottom = height.saturating_sub(status_height + 1);
    if text_bottom <= text_top {
        return None;
    }

    let text_width = width.saturating_sub(SCROLLBAR_WIDTH);
    let horizontal_scroll_y = (!state.word_wrap && text_bottom > text_top + SCROLLBAR_WIDTH)
        .then(|| text_bottom.saturating_sub(SCROLLBAR_WIDTH));
    let editor_bottom = horizontal_scroll_y.unwrap_or(text_bottom);
    if editor_bottom <= text_top {
        return None;
    }

    let text_height = editor_bottom - text_top;
    Some(EditorMetrics {
        text_top,
        text_bottom,
        editor_bottom,
        text_width,
        text_height,
        editor_text_width: text_width.saturating_sub(TEXT_MARGIN_X + 4),
        visible_line_count: text_height
            .saturating_sub(TEXT_MARGIN_Y * 2)
            .checked_div(EDITOR_LINE_HEIGHT)
            .unwrap_or(0)
            .max(1),
        horizontal_scroll_y,
    })
}

fn max_first_visible_line(app: &BlitzApp, visible_lines: usize) -> usize {
    app.document()
        .line_count()
        .saturating_sub(visible_lines.max(1))
}

fn max_visible_line_len(app: &BlitzApp, first_line: usize, visible_lines: usize) -> usize {
    (first_line..first_line.saturating_add(visible_lines))
        .filter_map(|line| app.document().line_range(line))
        .map(|range| range.len())
        .max()
        .unwrap_or(0)
}

fn visible_byte_capacity(fonts: &FontStack, editor_text_width: usize) -> usize {
    let average_char_width = fonts.measure("m", TextRole::Editor).max(1);
    editor_text_width.saturating_div(average_char_width).max(1)
}

fn offset_with_delta(offset: usize, delta: isize) -> usize {
    if delta >= 0 {
        offset.saturating_add(delta as usize)
    } else {
        offset.saturating_sub(delta.unsigned_abs())
    }
}

fn scroll_thumb(
    track_start: usize,
    track_len: usize,
    content_len: usize,
    visible_len: usize,
    offset: usize,
) -> (usize, usize) {
    if track_len == 0 || content_len <= visible_len || visible_len == 0 {
        return (track_start, track_len.max(1));
    }

    let thumb_len = ((track_len as u128 * visible_len as u128) / content_len as u128)
        .try_into()
        .unwrap_or(track_len)
        .clamp(MIN_SCROLL_THUMB.min(track_len), track_len);
    let max_offset = content_len.saturating_sub(visible_len).max(1);
    let travel = track_len.saturating_sub(thumb_len);
    let thumb_offset = ((travel as u128 * offset.min(max_offset) as u128) / max_offset as u128)
        .try_into()
        .unwrap_or(travel);
    (track_start + thumb_offset, thumb_len)
}

fn offset_for_thumb(
    track_start: usize,
    track_len: usize,
    thumb_len: usize,
    content_len: usize,
    visible_len: usize,
    thumb_position: usize,
) -> usize {
    if track_len <= thumb_len || content_len <= visible_len || visible_len == 0 {
        return 0;
    }

    let travel = track_len - thumb_len;
    let max_offset = content_len - visible_len;
    let thumb_offset = thumb_position.saturating_sub(track_start).min(travel);
    ((thumb_offset as u128 * max_offset as u128) / travel as u128)
        .try_into()
        .unwrap_or(max_offset)
}

fn draw_vertical_scrollbar(
    canvas: &mut Canvas,
    app: &BlitzApp,
    gui_state: &GuiState,
    metrics: EditorMetrics,
) {
    let (thumb_y, thumb_height) = scroll_thumb(
        metrics.text_top,
        metrics.text_height,
        app.document().line_count(),
        metrics.visible_line_count,
        gui_state.first_visible_line,
    );
    canvas.fill_rect(
        metrics.text_width + 4,
        thumb_y,
        SCROLLBAR_WIDTH.saturating_sub(8),
        thumb_height,
        COLOR_SCROLL_THUMB,
    );
}

fn draw_horizontal_scrollbar(
    canvas: &mut Canvas,
    app: &BlitzApp,
    gui_state: &GuiState,
    metrics: EditorMetrics,
    scroll_y: usize,
) {
    let max_line_len = max_visible_line_len(
        app,
        gui_state.first_visible_line,
        metrics.visible_line_count,
    );
    let visible_bytes = visible_byte_capacity(&gui_state.fonts, metrics.editor_text_width);
    let (thumb_x, thumb_width) = scroll_thumb(
        0,
        metrics.text_width,
        max_line_len.max(visible_bytes),
        visible_bytes,
        gui_state.horizontal_offset,
    );
    canvas.fill_rect(
        thumb_x,
        scroll_y + 4,
        thumb_width,
        SCROLLBAR_WIDTH.saturating_sub(8),
        COLOR_SCROLL_THUMB,
    );
}

fn draw_text_area(
    canvas: &mut Canvas,
    app: &BlitzApp,
    state: &NotepadUiState,
    gui_state: &GuiState,
) {
    let Some(metrics) = editor_metrics(state, canvas.width, canvas.height) else {
        return;
    };
    let fonts = &gui_state.fonts;

    canvas.fill_rect(
        0,
        metrics.text_top,
        metrics.text_width,
        metrics.text_height,
        COLOR_TEXT_AREA,
    );
    canvas.fill_rect(
        metrics.text_width,
        metrics.text_top,
        SCROLLBAR_WIDTH,
        metrics.text_height,
        COLOR_SCROLLBAR,
    );
    canvas.line(
        metrics.text_width,
        metrics.text_top,
        metrics.text_width,
        metrics.text_bottom,
        COLOR_BORDER,
    );
    draw_vertical_scrollbar(canvas, app, gui_state, metrics);

    if let Some(scroll_y) = metrics.horizontal_scroll_y {
        canvas.fill_rect(
            0,
            scroll_y,
            metrics.text_width,
            SCROLLBAR_WIDTH,
            COLOR_SCROLLBAR,
        );
        canvas.line(0, scroll_y, metrics.text_width, scroll_y, COLOR_BORDER);
        draw_horizontal_scrollbar(canvas, app, gui_state, metrics, scroll_y);
    }

    let first_visible_line = gui_state
        .first_visible_line
        .min(max_first_visible_line(app, metrics.visible_line_count));
    let visible_lines = app.document().visible_lines_at(
        first_visible_line,
        metrics.visible_line_count,
        gui_state.horizontal_offset,
    );
    for (index, line) in visible_lines.iter().enumerate() {
        let y = metrics.text_top + TEXT_MARGIN_Y + index * EDITOR_LINE_HEIGHT;
        let text = text_prefix_for_width(
            fonts,
            &line.text,
            TextRole::Editor,
            metrics.editor_text_width,
        );
        canvas.text(TEXT_MARGIN_X, y, text, COLOR_TEXT, TextRole::Editor, fonts);
    }

    if visible_lines.is_empty() {
        draw_caret(canvas, TEXT_MARGIN_X, metrics.text_top + TEXT_MARGIN_Y);
    } else if let Some(visible_index) = state
        .caret_line
        .checked_sub(1)
        .and_then(|line| line.checked_sub(first_visible_line))
        .filter(|index| *index < visible_lines.len())
    {
        let line = &visible_lines[visible_index];
        let local_offset = app
            .caret_offset()
            .saturating_sub(line.byte_range.start)
            .min(line.text.len());
        let prefix = line.text.get(..local_offset).unwrap_or(&line.text);
        let visible_prefix =
            text_prefix_for_width(fonts, prefix, TextRole::Editor, metrics.editor_text_width);
        let caret_x = TEXT_MARGIN_X + fonts.measure(visible_prefix, TextRole::Editor);
        let y = metrics.text_top + TEXT_MARGIN_Y + visible_index * EDITOR_LINE_HEIGHT;
        draw_caret(canvas, caret_x, y);
    }
}

fn text_prefix_for_width<'a>(
    fonts: &FontStack,
    text: &'a str,
    role: TextRole,
    max_width: usize,
) -> &'a str {
    let limit = max_width as f32;
    let mut width = 0.0f32;
    for (index, character) in text.char_indices() {
        width += fonts.measure_char(character, role);
        if width > limit {
            return &text[..index];
        }
    }
    text
}

fn draw_caret(canvas: &mut Canvas, x: usize, y: usize) {
    canvas.fill_rect(x, y, 2, CARET_HEIGHT, COLOR_CARET);
}

fn text_offset_for_point(
    app: &BlitzApp,
    gui_state: &GuiState,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
) -> Option<usize> {
    let state = app.ui_state().ok()?;
    let metrics = editor_metrics(&state, width, height)?;
    if y < metrics.text_top + TEXT_MARGIN_Y || y >= metrics.editor_bottom || x >= metrics.text_width
    {
        return None;
    }

    let line_index = (y - metrics.text_top - TEXT_MARGIN_Y) / EDITOR_LINE_HEIGHT;
    let first_visible_line = gui_state
        .first_visible_line
        .min(max_first_visible_line(app, metrics.visible_line_count));
    let visible_lines = app.document().visible_lines_at(
        first_visible_line,
        metrics.visible_line_count,
        gui_state.horizontal_offset,
    );
    if visible_lines.is_empty() {
        return Some(0);
    }

    let line = visible_lines
        .get(line_index)
        .or_else(|| visible_lines.last())?;
    let target_x = x.saturating_sub(TEXT_MARGIN_X) as f32;
    Some(offset_for_x(
        &gui_state.fonts,
        &line.text,
        line.byte_range.start,
        target_x,
    ))
}

fn offset_for_x(fonts: &FontStack, line: &str, line_start: usize, target_x: f32) -> usize {
    let mut cursor = 0.0f32;
    for (index, character) in line.char_indices() {
        let advance = fonts.measure_char(character, TextRole::Editor);
        if target_x < cursor + advance / 2.0 {
            return line_start + index;
        }
        cursor += advance;
    }
    line_start + line.len()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MenuTitle {
    index: usize,
    label: &'static str,
    x: usize,
    width: usize,
}

fn menu_titles(fonts: &FontStack) -> Vec<MenuTitle> {
    let mut x = 4usize;
    MENU_BAR
        .iter()
        .enumerate()
        .map(|(index, label)| {
            let width = fonts.measure(label, TextRole::Ui) + 20;
            let title = MenuTitle {
                index,
                label,
                x,
                width,
            };
            x += width;
            title
        })
        .collect()
}

fn menu_title_hit(fonts: &FontStack, x: usize, y: usize) -> Option<usize> {
    if !(menu_top()..menu_top() + MENU_HEIGHT).contains(&y) {
        return None;
    }
    menu_titles(fonts)
        .into_iter()
        .find(|title| x >= title.x && x < title.x + title.width)
        .map(|title| title.index)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MenuRow {
    label: String,
    shortcut: Option<String>,
    enabled: bool,
    checked: bool,
    separator_before: bool,
    indent: usize,
    has_children: bool,
}

fn menu_rows(app: &BlitzApp, menu_index: usize) -> Vec<MenuRow> {
    let Ok(state) = app.ui_state() else {
        return Vec::new();
    };
    let Some(items) = state.menu(MENU_BAR[menu_index]) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    let mut separator_before = false;
    append_menu_items(&mut rows, &items, 0, &mut separator_before);
    rows
}

fn append_menu_items(
    rows: &mut Vec<MenuRow>,
    items: &[MenuItem],
    indent: usize,
    separator_before: &mut bool,
) {
    for item in items {
        rows.push(MenuRow {
            label: item.label.to_owned(),
            shortcut: item.shortcut.map(ToOwned::to_owned),
            enabled: item.enabled,
            checked: item.checked,
            separator_before: *separator_before,
            indent,
            has_children: !item.children.is_empty(),
        });
        *separator_before = false;

        if !item.children.is_empty() {
            let mut child_separator = false;
            append_menu_items(rows, &item.children, indent + 18, &mut child_separator);
        }
        if item.separator_after {
            *separator_before = true;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MenuMetrics {
    width: usize,
    height: usize,
    shortcut_x: usize,
}

fn menu_metrics(fonts: &FontStack, rows: &[MenuRow]) -> MenuMetrics {
    let max_label = rows
        .iter()
        .map(|row| fonts.measure(&row.label, TextRole::Ui) + row.indent)
        .max()
        .unwrap_or(100);
    let max_shortcut = rows
        .iter()
        .filter_map(|row| row.shortcut.as_ref())
        .map(|shortcut| fonts.measure(shortcut, TextRole::Ui))
        .max()
        .unwrap_or(0);
    let separators = rows.iter().filter(|row| row.separator_before).count();
    let shortcut_x = 44 + max_label + 30;
    MenuMetrics {
        width: (shortcut_x + max_shortcut + 18).max(180),
        height: rows.len() * MENU_ITEM_HEIGHT + separators * MENU_SEPARATOR_HEIGHT + 8,
        shortcut_x,
    }
}

fn menu_row_hit(
    app: &BlitzApp,
    fonts: &FontStack,
    menu_index: usize,
    x: usize,
    y: usize,
) -> Option<MenuRow> {
    let title = menu_titles(fonts)
        .into_iter()
        .find(|title| title.index == menu_index)?;
    let rows = menu_rows(app, menu_index);
    let metrics = menu_metrics(fonts, &rows);
    if x < title.x || x >= title.x + metrics.width || y < dropdown_top() {
        return None;
    }

    let mut row_y = dropdown_top() + 4;
    for row in rows {
        if row.separator_before {
            row_y += MENU_SEPARATOR_HEIGHT;
        }
        if y >= row_y && y < row_y + MENU_ITEM_HEIGHT {
            return Some(row);
        }
        row_y += MENU_ITEM_HEIGHT;
    }
    None
}

fn menu_top() -> usize {
    0
}

fn dropdown_top() -> usize {
    MENU_HEIGHT
}

fn text_top() -> usize {
    MENU_HEIGHT + 1
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum TextRole {
    Ui,
    Editor,
}

#[derive(Clone)]
struct LoadedFont {
    name: String,
    font: FontArc,
}

struct FontStack {
    fonts: Vec<LoadedFont>,
    glyph_cache: RefCell<HashMap<GlyphCacheKey, GlyphMetrics>>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct GlyphCacheKey {
    character: char,
    role: TextRole,
}

#[derive(Clone, Copy, Debug)]
struct GlyphMetrics {
    font_index: usize,
    glyph_id: GlyphId,
    advance: f32,
}

impl FontStack {
    fn load() -> Result<Self> {
        let mut database = Database::new();
        database.load_system_fonts();
        let mut fonts = Vec::new();

        for family in FONT_FAMILIES {
            if let Some(font) = load_font_family(&database, family) {
                if !fonts
                    .iter()
                    .any(|loaded: &LoadedFont| loaded.name == font.name)
                {
                    fonts.push(font);
                }
            }
        }

        if fonts.is_empty() {
            for face in database.faces() {
                let name = face
                    .families
                    .first()
                    .map(|(name, _)| name.as_str())
                    .unwrap_or("System UI");
                if let Some(font) = load_font_id(&database, face.id, face.index, name) {
                    fonts.push(font);
                    break;
                }
            }
        }

        if fonts.is_empty() {
            return Err(BlitzError::Window("no usable system font found".to_owned()));
        }

        Ok(Self {
            fonts,
            glyph_cache: RefCell::new(HashMap::new()),
        })
    }

    fn description(&self) -> String {
        self.fonts
            .iter()
            .take(2)
            .map(|font| font.name.as_str())
            .collect::<Vec<_>>()
            .join(" / ")
    }

    fn size(&self, role: TextRole) -> f32 {
        match role {
            TextRole::Ui => UI_FONT_SIZE,
            TextRole::Editor => EDITOR_FONT_SIZE,
        }
    }

    fn measure(&self, text: &str, role: TextRole) -> usize {
        self.measure_text(text, role).round() as usize
    }

    fn measure_text(&self, text: &str, role: TextRole) -> f32 {
        text.chars()
            .map(|character| self.measure_char(character, role))
            .sum()
    }

    fn measure_char(&self, character: char, role: TextRole) -> f32 {
        if character == '\t' {
            return self.measure_char(' ', role) * 4.0;
        }
        self.glyph_metrics(character, role).advance
    }

    fn draw_text(
        &self,
        canvas: &mut Canvas,
        x: f32,
        baseline: f32,
        text: &str,
        color: u32,
        role: TextRole,
    ) {
        let size = self.size(role);
        let mut cursor = x;
        for character in text.chars() {
            if character == '\t' {
                cursor += self.measure_char(character, role);
                continue;
            }
            let metrics = self.glyph_metrics(character, role);
            let font = &self.fonts[metrics.font_index].font;
            let glyph = metrics
                .glyph_id
                .with_scale_and_position(size, point(cursor, baseline));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|glyph_x, glyph_y, coverage| {
                    canvas.blend_pixel(
                        bounds.min.x as i32 + glyph_x as i32,
                        bounds.min.y as i32 + glyph_y as i32,
                        color,
                        coverage,
                    );
                });
            }
            cursor += metrics.advance;
        }
    }

    fn glyph_metrics(&self, character: char, role: TextRole) -> GlyphMetrics {
        let key = GlyphCacheKey { character, role };
        if let Some(metrics) = self.glyph_cache.borrow().get(&key) {
            return *metrics;
        }

        let (font_index, glyph_id) = self.font_index_for(character);
        let size = self.size(role);
        let advance = self.fonts[font_index]
            .font
            .as_scaled(PxScale::from(size))
            .h_advance(glyph_id)
            .max(size * 0.35);
        let metrics = GlyphMetrics {
            font_index,
            glyph_id,
            advance,
        };
        self.glyph_cache.borrow_mut().insert(key, metrics);
        metrics
    }

    fn font_index_for(&self, character: char) -> (usize, GlyphId) {
        self.fonts
            .iter()
            .enumerate()
            .find_map(|(index, loaded)| {
                let glyph_id = loaded.font.glyph_id(character);
                (glyph_id != GlyphId(0)).then_some((index, glyph_id))
            })
            .unwrap_or_else(|| (0, self.fonts[0].font.glyph_id(character)))
    }
}

fn load_font_family(database: &Database, family: &str) -> Option<LoadedFont> {
    let query = Query {
        families: &[Family::Name(family)],
        ..Query::default()
    };
    let id = database.query(&query)?;
    let face = database.face(id)?;
    load_font_id(database, id, face.index, family)
}

fn load_font_id(database: &Database, id: fontdb::ID, index: u32, name: &str) -> Option<LoadedFont> {
    database
        .with_face_data(id, |data, _face_index| {
            FontVec::try_from_vec_and_index(data.to_vec(), index)
                .ok()
                .map(FontArc::from)
                .map(|font| LoadedFont {
                    name: name.to_owned(),
                    font,
                })
        })
        .flatten()
}

struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}

impl Canvas {
    fn new(width: usize, height: usize, color: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![color; width * height],
        }
    }

    fn into_frame(self) -> RenderFrame {
        RenderFrame {
            width: self.width,
            height: self.height,
            pixels: self.pixels,
        }
    }

    fn fill_rect(&mut self, x: usize, y: usize, width: usize, height: usize, color: u32) {
        let end_x = (x + width).min(self.width);
        let end_y = (y + height).min(self.height);
        for yy in y.min(self.height)..end_y {
            let row_start = yy * self.width;
            for xx in x.min(self.width)..end_x {
                self.pixels[row_start + xx] = color;
            }
        }
    }

    fn rect(&mut self, x: usize, y: usize, width: usize, height: usize, color: u32) {
        self.line(x, y, x + width, y, color);
        self.line(x, y + height, x + width, y + height, color);
        self.line(x, y, x, y + height, color);
        self.line(x + width, y, x + width, y + height, color);
    }

    fn line(&mut self, x1: usize, y1: usize, x2: usize, y2: usize, color: u32) {
        if y1 == y2 {
            self.fill_rect(x1.min(x2), y1, x1.abs_diff(x2).max(1), 1, color);
        } else if x1 == x2 {
            self.fill_rect(x1, y1.min(y2), 1, y1.abs_diff(y2).max(1), color);
        }
    }

    fn text(
        &mut self,
        x: usize,
        y: usize,
        text: &str,
        color: u32,
        role: TextRole,
        fonts: &FontStack,
    ) {
        let baseline = y as f32
            + match role {
                TextRole::Ui => 15.0,
                TextRole::Editor => 19.0,
            };
        fonts.draw_text(self, x as f32, baseline, text, color, role);
    }

    fn blend_pixel(&mut self, x: i32, y: i32, color: u32, coverage: f32) {
        if x < 0 || y < 0 || coverage <= 0.0 {
            return;
        }
        let x = x as usize;
        let y = y as usize;
        if x >= self.width || y >= self.height {
            return;
        }
        let index = y * self.width + x;
        self.pixels[index] = blend(self.pixels[index], color, coverage.clamp(0.0, 1.0));
    }
}

fn blend(destination: u32, source: u32, alpha: f32) -> u32 {
    let source_r = ((source >> 16) & 0xff) as f32;
    let source_g = ((source >> 8) & 0xff) as f32;
    let source_b = (source & 0xff) as f32;
    let destination_r = ((destination >> 16) & 0xff) as f32;
    let destination_g = ((destination >> 8) & 0xff) as f32;
    let destination_b = (destination & 0xff) as f32;

    let inverse = 1.0 - alpha;
    let r = source_r * alpha + destination_r * inverse;
    let g = source_g * alpha + destination_g * inverse;
    let b = source_b * alpha + destination_b * inverse;
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

#[cfg(test)]
mod tests {
    use crate::settings::EditorSettings;

    use super::*;

    #[test]
    fn renderer_produces_nonblank_notepad_shell() {
        let app = BlitzApp::new(EditorSettings::default());
        let frame = render_frame(&app, 640, 400).expect("render");

        assert_eq!(frame.width, 640);
        assert_eq!(frame.height, 400);
        assert!(frame.pixels.iter().any(|pixel| *pixel == COLOR_TEXT_AREA));
        assert!(frame.pixels.iter().any(|pixel| *pixel == COLOR_STATUS));
        assert!(frame.pixels.iter().any(|pixel| *pixel != COLOR_WINDOW));
    }

    #[test]
    fn mouse_point_maps_to_document_offset() {
        let mut gui_state = GuiState::new().expect("gui state");
        let mut app = BlitzApp::new(EditorSettings::default());
        app.insert_text(&"abc\ndef\n".repeat(32)).expect("insert");

        let x_for_c = TEXT_MARGIN_X + gui_state.fonts.measure_text("ab", TextRole::Editor) as usize;
        let offset = text_offset_for_point(
            &app,
            &gui_state,
            640,
            400,
            x_for_c,
            text_top() + TEXT_MARGIN_Y,
        )
        .expect("offset");
        assert_eq!(offset, 2);

        let second_line = text_offset_for_point(
            &app,
            &gui_state,
            640,
            400,
            TEXT_MARGIN_X + gui_state.fonts.measure_text("d", TextRole::Editor) as usize,
            text_top() + TEXT_MARGIN_Y + EDITOR_LINE_HEIGHT,
        )
        .expect("offset");
        assert_eq!(second_line, "abc\n".len() + 1);

        gui_state.first_visible_line = 1;
        let scrolled_top_line = text_offset_for_point(
            &app,
            &gui_state,
            640,
            400,
            TEXT_MARGIN_X,
            text_top() + TEXT_MARGIN_Y,
        )
        .expect("scrolled offset");
        assert_eq!(scrolled_top_line, "abc\n".len());
    }

    #[test]
    fn horizontal_scroll_offsets_hit_testing() {
        let mut gui_state = GuiState::new().expect("gui state");
        let mut app = BlitzApp::new(EditorSettings::default());
        app.insert_text("abcdef").expect("insert");
        gui_state.horizontal_offset = 3;

        let offset = text_offset_for_point(
            &app,
            &gui_state,
            640,
            400,
            TEXT_MARGIN_X,
            text_top() + TEXT_MARGIN_Y,
        )
        .expect("offset");

        assert_eq!(offset, 3);
    }

    #[test]
    fn scrollbar_clicks_move_viewport() {
        let mut app = BlitzApp::new(EditorSettings::default());
        let mut gui_state = GuiState::new().expect("gui state");
        app.insert_text(
            &(0..100)
                .map(|line| format!("line {line}\n"))
                .collect::<String>(),
        )
        .expect("insert");

        let state = app.ui_state().expect("ui state");
        let metrics = editor_metrics(&state, 640, 400).expect("metrics");
        let clicked = handle_scrollbar_click(
            &app,
            &mut gui_state,
            640,
            400,
            metrics.text_width + 2,
            metrics.editor_bottom - 2,
        )
        .expect("click");

        assert!(clicked);
        assert!(gui_state.first_visible_line > 0);
    }

    #[test]
    fn horizontal_scrollbar_click_and_drag_move_offset() {
        let mut app = BlitzApp::new(EditorSettings::default());
        let mut gui_state = GuiState::new().expect("gui state");
        app.insert_text(&"abcdef".repeat(400)).expect("insert");

        let state = app.ui_state().expect("ui state");
        let metrics = editor_metrics(&state, 640, 400).expect("metrics");
        let scroll_y = metrics.horizontal_scroll_y.expect("horizontal scrollbar");

        let clicked = handle_scrollbar_click(
            &app,
            &mut gui_state,
            640,
            400,
            metrics.text_width - 2,
            scroll_y + 8,
        )
        .expect("track click");
        assert!(clicked);
        assert!(gui_state.horizontal_offset > 0);

        gui_state.horizontal_offset = 0;
        handle_scrollbar_click(&app, &mut gui_state, 640, 400, 1, scroll_y + 8)
            .expect("thumb click");
        assert!(matches!(
            gui_state.scroll_drag,
            Some(ScrollDrag::Horizontal { .. })
        ));

        update_scroll_drag(
            &app,
            &mut gui_state,
            640,
            400,
            metrics.text_width - 2,
            scroll_y + 8,
        )
        .expect("drag");

        assert!(gui_state.horizontal_offset > 0);
    }

    #[test]
    fn menu_hit_testing_finds_file_new() {
        let fonts = FontStack::load().expect("fonts");
        let app = BlitzApp::new(EditorSettings::default());
        let title = menu_titles(&fonts)[0].clone();

        assert_eq!(menu_title_hit(&fonts, title.x + 1, menu_top() + 4), Some(0));
        let row = menu_row_hit(&app, &fonts, 0, title.x + 20, dropdown_top() + 10).expect("row");
        assert_eq!(row.label, "New");
    }

    #[test]
    fn font_stack_loads_a_system_ui_font() {
        let fonts = FontStack::load().expect("fonts");
        assert!(!fonts.description().is_empty());
    }

    #[test]
    fn font_stack_reuses_glyph_metrics() {
        let fonts = FontStack::load().expect("fonts");

        assert_eq!(fonts.glyph_cache.borrow().len(), 0);
        let first_width = fonts.measure("ababa", TextRole::Editor);
        let cache_len = fonts.glyph_cache.borrow().len();
        let second_width = fonts.measure("ababa", TextRole::Editor);

        assert_eq!(first_width, second_width);
        assert_eq!(fonts.glyph_cache.borrow().len(), cache_len);
        assert!(cache_len <= 2);
    }

    #[test]
    fn frame_signature_tracks_viewport_changes() {
        let app = BlitzApp::new(EditorSettings::default());
        let mut gui_state = GuiState::new().expect("gui state");
        let state = app.ui_state().expect("ui state");

        let initial = frame_signature(&app, &state, 640, 400, &gui_state);
        let unchanged = frame_signature(&app, &state, 640, 400, &gui_state);
        gui_state.first_visible_line = 1;
        let scrolled = frame_signature(&app, &state, 640, 400, &gui_state);

        assert_eq!(initial, unchanged);
        assert_ne!(initial, scrolled);
    }

    #[test]
    fn startup_screenshot_is_written_as_png() {
        let app = BlitzApp::new(EditorSettings::default());
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("startup.png");

        save_startup_screenshot(&app, &path).expect("screenshot");
        let bytes = std::fs::read(path).expect("read screenshot");

        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(bytes.len() > 1024);
    }

    #[test]
    fn startup_frame_contains_required_chrome_regions() {
        let app = BlitzApp::new(EditorSettings::default());
        let frame = render_frame(&app, DEFAULT_WIDTH, DEFAULT_HEIGHT).expect("render");

        assert_eq!(
            frame.pixels[pixel_index(&frame, 5, menu_top() + 5)],
            COLOR_WINDOW
        );
        assert_eq!(
            frame.pixels[pixel_index(&frame, 5, text_top() + 5)],
            COLOR_TEXT_AREA
        );
        assert!(matches!(
            frame.pixels[pixel_index(&frame, DEFAULT_WIDTH - 5, text_top() + 20)],
            COLOR_SCROLLBAR | COLOR_SCROLL_THUMB
        ));
        assert_eq!(
            frame.pixels[pixel_index(&frame, 5, DEFAULT_HEIGHT - STATUS_HEIGHT + 5)],
            COLOR_STATUS
        );
        assert!(frame.pixels.iter().any(|pixel| *pixel == COLOR_CARET));
    }

    #[test]
    fn long_editor_text_prefix_is_clipped_to_available_width() {
        let fonts = FontStack::load().expect("fonts");
        let text = "日本語abcdef".repeat(128);
        let max_width = fonts.measure("日本語abc", TextRole::Editor);

        let prefix = text_prefix_for_width(&fonts, &text, TextRole::Editor, max_width);

        assert!(prefix.len() < text.len());
        assert!(fonts.measure(prefix, TextRole::Editor) <= max_width);
        assert!(text.is_char_boundary(prefix.len()));
    }

    #[test]
    fn menu_commands_mutate_editor_state() {
        let mut app = BlitzApp::new(EditorSettings::default());
        let mut gui_state = GuiState::new().expect("gui state");

        app.insert_text("alpha").expect("insert");
        execute_menu_row(
            &mut app,
            &mut gui_state,
            0,
            MenuRow {
                label: "New".to_owned(),
                shortcut: Some("Ctrl+N".to_owned()),
                enabled: true,
                checked: false,
                separator_before: false,
                indent: 0,
                has_children: false,
            },
        )
        .expect("new");
        assert_eq!(app.document().text_lossy(), "");

        execute_menu_row(
            &mut app,
            &mut gui_state,
            3,
            MenuRow {
                label: "Status Bar".to_owned(),
                shortcut: None,
                enabled: true,
                checked: true,
                separator_before: false,
                indent: 0,
                has_children: false,
            },
        )
        .expect("status");
        assert!(!app.ui_state().expect("ui").status_bar_visible);

        execute_menu_row(
            &mut app,
            &mut gui_state,
            2,
            MenuRow {
                label: "Word Wrap".to_owned(),
                shortcut: None,
                enabled: true,
                checked: false,
                separator_before: false,
                indent: 0,
                has_children: false,
            },
        )
        .expect("word wrap");
        assert!(app.ui_state().expect("ui").word_wrap);
    }

    #[test]
    fn transient_messages_do_not_cover_text_area() {
        let app = BlitzApp::new(EditorSettings::default());
        let mut gui_state = GuiState::new().expect("gui state");
        gui_state.set_message("New document");

        let state = app.ui_state().expect("ui state");
        let frame =
            render_frame_with_state(&app, &state, DEFAULT_WIDTH, DEFAULT_HEIGHT, &gui_state)
                .expect("render");

        assert_eq!(
            frame.pixels[pixel_index(&frame, TEXT_MARGIN_X + 24, text_top() + TEXT_MARGIN_Y + 4)],
            COLOR_TEXT_AREA
        );
    }

    #[test]
    fn find_next_uses_persisted_search() {
        let mut app = BlitzApp::new(EditorSettings::default());
        let mut gui_state = GuiState::new().expect("gui state");
        app.insert_text("one two one").expect("insert");
        gui_state.last_search = Some(SearchSpec {
            query: "one".to_owned(),
            match_case: true,
        });

        find_again(&mut app, &mut gui_state, true).expect("find");
        assert_eq!(app.selected_range(), Some(0..3));
        find_again(&mut app, &mut gui_state, true).expect("find");
        assert_eq!(app.selected_range(), Some(8..11));
    }

    #[test]
    fn replace_dialog_can_replace_all() {
        let mut app = BlitzApp::new(EditorSettings::default());
        let mut gui_state = GuiState::new().expect("gui state");
        app.insert_text("one one").expect("insert");
        gui_state.dialog = Some(DialogState::Replace {
            query: "one".to_owned(),
            replacement: "two".to_owned(),
            active_field: ReplaceField::Find,
            match_case: true,
        });

        accept_dialog(&mut app, &mut gui_state, true).expect("replace all");
        assert_eq!(app.document().text_lossy(), "two two");
    }

    fn pixel_index(frame: &RenderFrame, x: usize, y: usize) -> usize {
        y * frame.width + x
    }
}
