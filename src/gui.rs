use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::BufWriter;
#[cfg(target_os = "windows")]
use std::num::NonZeroIsize;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::sync::{
    mpsc::{self, Receiver, TryRecvError},
    Arc,
};
use std::thread;
use std::time::Duration;

use ab_glyph::{point, Font, FontArc, FontVec, GlyphId as AbGlyphId, PxScale, ScaleFont};
use arboard::Clipboard;
use fontdb::{Database, Family, Query};
use minifb::{InputCallback, Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};
use pixels::raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, Win32WindowHandle, WindowHandle, WindowsDisplayHandle,
};
use pixels::{Pixels, PixelsBuilder, SurfaceTexture};
use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
use swash::{
    scale::{image::Content, Render, ScaleContext, Source, StrikeWith},
    shape::{Direction, ShapeContext},
    text::Script,
    FontRef, GlyphId as SwashGlyphId,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::{
    Foundation::{HWND, POINT, RECT},
    UI::Input::{
        Ime::{
            ImmGetCompositionStringW, ImmGetContext, ImmGetOpenStatus, ImmReleaseContext,
            ImmSetCompositionWindow, CFS_FORCE_POSITION, COMPOSITIONFORM, GCS_COMPSTR,
        },
        KeyboardAndMouse::{GetKeyState, VK_CONTROL},
    },
};

use crate::document::VisibleLine;
use crate::ui::{MenuItem, NotepadUiState, MENU_BAR};
use crate::{BlitzApp, BlitzError, Document, Result};

const DEFAULT_WIDTH: usize = 960;
const DEFAULT_HEIGHT: usize = 640;
const FIND_WINDOW_WIDTH: usize = 430;
const FIND_WINDOW_HEIGHT: usize = 140;
const GO_TO_WINDOW_WIDTH: usize = 250;
const GO_TO_WINDOW_HEIGHT: usize = 100;
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
const FIND_FONT_SIZE: f32 = 13.0;
const EDITOR_FONT_SIZE: f32 = 18.0;
const EDITOR_BASELINE_OFFSET: usize = 19;
const EMOJI_FONT_SCALE: f32 = 0.68;
const WHEEL_LINES: isize = 3;
const HORIZONTAL_WHEEL_BYTES: isize = 96;
const MIN_SCROLL_THUMB: usize = 32;
const REVEAL_LINE_INDEX_BUDGET_BYTES: usize = 4 * 1024 * 1024;
const PRINT_LINE_CHUNK_BYTES: usize = 64 * 1024;

const COLOR_WINDOW: u32 = 0x00f0f0f0;
const COLOR_TEXT_AREA: u32 = 0x00ffffff;
const COLOR_STATUS: u32 = 0x00f0f0f0;
const COLOR_BORDER: u32 = 0x00d4d4d4;
const COLOR_MENU_OPEN: u32 = 0x00dbeeff;
const COLOR_DROPDOWN: u32 = 0x00f8f8f8;
const COLOR_BUTTON: u32 = 0x00e1e1e1;
const COLOR_FOCUS_BORDER: u32 = 0x000078d7;
const COLOR_GROUP_BOX: u32 = 0x00dddddd;
const COLOR_SCROLLBAR: u32 = 0x00e6e6e6;
const COLOR_SCROLL_THUMB: u32 = 0x00b8b8b8;
const COLOR_SELECTION: u32 = 0x00cce8ff;
const COLOR_TEXT: u32 = 0x00000000;
const COLOR_SELECTED_TEXT: u32 = 0x00ffffff;
const COLOR_DISABLED_TEXT: u32 = 0x00808080;
const COLOR_CARET: u32 = 0x00000000;

const FONT_FAMILIES: &[&str] = &[
    "Segoe UI",
    "Segoe UI Emoji",
    "Segoe UI Symbol",
    "Segoe Fluent Icons",
    "Segoe UI Historic",
    "Aptos",
    "Aptos Display",
    "Aptos Text",
    "Yu Gothic UI",
    "Yu Gothic",
    "YuGothic",
    "Hiragino Sans",
    "Hiragino Kaku Gothic ProN",
    "Apple SD Gothic Neo",
    "Apple Color Emoji",
    "Noto Color Emoji",
    "Noto Emoji",
    "Twemoji Mozilla",
    "Symbola",
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
    let mut presenter = FramePresenter::new(&window, DEFAULT_WIDTH, DEFAULT_HEIGHT);
    let mut last_title = String::new();

    while window.is_open() && !window.is_key_down(Key::Escape) {
        app.set_clipboard_has_text(clipboard_has_text(&gui_state.clipboard));
        if handle_mouse(&window, &mut app, &mut gui_state)? {
            break;
        }
        handle_keys(&input_queue, &window, &mut app, &mut gui_state)?;
        handle_text_input(&input_queue, &window, &mut app, &mut gui_state)?;
        handle_find_window(&mut app, &mut gui_state)?;
        handle_dialog_window(&mut app, &mut gui_state)?;
        poll_save_job(&mut app, &mut gui_state);
        poll_print_job(&mut gui_state);

        let (width, height) = window.get_size();
        let width = width.max(MIN_WIDTH);
        let height = height.max(MIN_HEIGHT);
        handle_scroll_wheel(&window, &app, &mut gui_state, width, height)?;
        gui_state.follow_caret_if_moved(&app, width, height)?;
        update_editor_ime_state(&window, &app, &mut gui_state, width, height);
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
        presenter.present(&mut window, frame)?;
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
    dialog: Option<DialogWindowState>,
    pending_info_dialog: Option<DialogState>,
    find_window: Option<FindWindowState>,
    last_search: Option<SearchSpec>,
    status_message: Option<String>,
    clipboard: String,
    mouse_was_down: bool,
    first_visible_line: usize,
    horizontal_offset: usize,
    last_caret_offset: usize,
    pending_reveal_selection: bool,
    scroll_drag: Option<ScrollDrag>,
    selection_anchor: Option<usize>,
    selection_focus: Option<usize>,
    mouse_selecting: bool,
    save_job: Option<SaveJob>,
    print_job: Option<PrintJob>,
    fonts: FontStack,
    wrapped_row_index: RefCell<Option<WrappedRowIndex>>,
    editor_ime_composition: String,
}

impl GuiState {
    fn new() -> Result<Self> {
        let fonts = FontStack::load()?;
        Ok(Self {
            active_menu: None,
            dialog: None,
            pending_info_dialog: None,
            find_window: None,
            last_search: None,
            status_message: None,
            clipboard: String::new(),
            mouse_was_down: false,
            first_visible_line: 0,
            horizontal_offset: 0,
            last_caret_offset: 0,
            pending_reveal_selection: false,
            scroll_drag: None,
            selection_anchor: None,
            selection_focus: None,
            mouse_selecting: false,
            save_job: None,
            print_job: None,
            fonts,
            wrapped_row_index: RefCell::new(None),
            editor_ime_composition: String::new(),
        })
    }

    fn set_message(&mut self, message: impl Into<String>) {
        self.status_message = Some(message.into());
    }

    fn status_text(&self) -> Option<&str> {
        if self.print_job.is_some() {
            Some("Printing...")
        } else if self.save_job.is_some() {
            Some("Saving...")
        } else {
            self.status_message.as_deref()
        }
    }

    fn reset_scroll(&mut self, app: &BlitzApp) {
        self.first_visible_line = 0;
        self.horizontal_offset = 0;
        self.wrapped_row_index.borrow_mut().take();
        self.last_caret_offset = app.caret_offset();
        self.pending_reveal_selection = false;
        self.status_message = None;
        self.selection_anchor = None;
        self.selection_focus = None;
        self.mouse_selecting = false;
        self.editor_ime_composition.clear();
    }

    fn scroll_vertical(&mut self, app: &BlitzApp, delta_lines: isize, metrics: EditorMetrics) {
        let max_first_line = max_first_visible_line_for_metrics(app, self, metrics);
        self.first_visible_line =
            offset_with_delta(self.first_visible_line, delta_lines).min(max_first_line);
        self.clamp_horizontal(app, metrics.visible_line_count);
    }

    fn scroll_horizontal(&mut self, app: &BlitzApp, delta_bytes: isize, visible_lines: usize) {
        self.horizontal_offset = offset_with_delta(self.horizontal_offset, delta_bytes);
        self.clamp_horizontal(app, visible_lines);
    }

    fn clamp_horizontal(&mut self, app: &BlitzApp, visible_lines: usize) {
        if app.settings().word_wrap {
            self.horizontal_offset = 0;
            return;
        }

        self.horizontal_offset = self.horizontal_offset.min(max_visible_line_len(
            app,
            self.first_visible_line,
            visible_lines,
        ));
    }

    fn reset_word_wrap_view(&mut self) {
        self.first_visible_line = 0;
        self.horizontal_offset = 0;
        self.wrapped_row_index.borrow_mut().take();
        self.editor_ime_composition.clear();
    }

    fn follow_caret_if_moved(&mut self, app: &BlitzApp, width: usize, height: usize) -> Result<()> {
        if self.last_caret_offset == app.caret_offset() && !self.pending_reveal_selection {
            return Ok(());
        }

        if let Some(metrics) = editor_metrics_for_app(app, width, height) {
            if self.pending_reveal_selection {
                if self.reveal_selection(app, metrics)? {
                    self.pending_reveal_selection = false;
                    self.last_caret_offset = app.caret_offset();
                }
                return Ok(());
            }

            if app.settings().word_wrap {
                let caret_row =
                    wrapped_visual_row_for_offset(app, self, metrics, app.caret_offset())?;
                if caret_row < self.first_visible_line {
                    self.first_visible_line = caret_row;
                } else if caret_row >= self.first_visible_line + metrics.visible_line_count {
                    self.first_visible_line = caret_row
                        .saturating_sub(metrics.visible_line_count)
                        .saturating_add(1);
                }
                self.horizontal_offset = 0;
            } else {
                let caret_line = app.document().line_for_offset(app.caret_offset())?;
                if caret_line < self.first_visible_line {
                    self.first_visible_line = caret_line;
                } else if caret_line >= self.first_visible_line + metrics.visible_line_count {
                    self.first_visible_line = caret_line
                        .saturating_sub(metrics.visible_line_count)
                        .saturating_add(1);
                }
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
            }
            self.first_visible_line = self
                .first_visible_line
                .min(max_first_visible_line_for_metrics(app, self, metrics));
            self.clamp_horizontal(app, metrics.visible_line_count);
        }

        self.last_caret_offset = app.caret_offset();
        Ok(())
    }

    fn reveal_selection(&mut self, app: &BlitzApp, metrics: EditorMetrics) -> Result<bool> {
        let Some(selection) = app.selected_range() else {
            return Ok(true);
        };

        let reveal_offset = selection.end.min(app.document().len());
        if !app.document().line_index_covers_offset(reveal_offset)
            && !app
                .document()
                .extend_line_index_towards_offset(reveal_offset, REVEAL_LINE_INDEX_BUDGET_BYTES)?
        {
            return Ok(false);
        }

        let selection_row = if app.settings().word_wrap {
            wrapped_visual_row_for_offset(app, self, metrics, reveal_offset)?
        } else {
            app.document().line_for_offset(reveal_offset)?
        };
        self.first_visible_line = selection_row
            .saturating_sub(metrics.visible_line_count / 2)
            .min(max_first_visible_line_for_metrics(app, self, metrics));
        self.center_horizontal_range(app, metrics, selection)?;
        self.clamp_horizontal(app, metrics.visible_line_count);
        Ok(true)
    }

    fn center_horizontal_range(
        &mut self,
        app: &BlitzApp,
        metrics: EditorMetrics,
        selection: std::ops::Range<usize>,
    ) -> Result<()> {
        let content_range = app
            .document()
            .line_content_range_for_offset(selection.start)?;
        let selection_start = selection
            .start
            .clamp(content_range.start, content_range.end);
        let selection_end = selection.end.min(content_range.end).max(selection_start);
        let relative_start = selection_start.saturating_sub(content_range.start);
        let relative_end = selection_end.saturating_sub(content_range.start);
        if app.settings().word_wrap {
            self.horizontal_offset = 0;
            return Ok(());
        }

        let visible_bytes = visible_byte_capacity(&self.fonts, metrics.editor_text_width);
        self.horizontal_offset =
            centered_offset_for_range(relative_start, relative_end, visible_bytes);
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct WrappedRowIndex {
    generation: u64,
    line_count: usize,
    editor_text_width: usize,
    row_starts: Vec<usize>,
    total_rows: usize,
}

struct SaveJob {
    receiver: Receiver<SaveJobResult>,
}

struct SaveJobResult {
    generation: u64,
    result: Result<u64>,
}

struct PrintJob {
    receiver: Receiver<PrintJobResult>,
}

struct PrintJobResult {
    result: Result<PrintOutcome>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrintOutcome {
    Submitted,
    Cancelled,
}

struct PrintRequest {
    document: Document,
    title: String,
}

enum FramePresenter {
    Gpu(GpuPresenter),
    Software,
}

impl FramePresenter {
    fn new(window: &Window, width: usize, height: usize) -> Self {
        GpuPresenter::new(window, width, height)
            .map(Self::Gpu)
            .unwrap_or_else(|error| {
                eprintln!("blitzpad: {error}; using software presentation");
                Self::Software
            })
    }

    fn present(&mut self, window: &mut Window, frame: &RenderFrame) -> Result<()> {
        if let Self::Gpu(gpu) = self {
            match gpu.present(frame) {
                Ok(()) => {
                    // GPU presentation owns the pixels surface; minifb still needs update() for input/state polling.
                    window.update();
                    return Ok(());
                }
                Err(error) => {
                    eprintln!("blitzpad: {error}; falling back to software presentation");
                    *self = Self::Software;
                }
            }
        }

        window
            .update_with_buffer(&frame.pixels, frame.width, frame.height)
            .map_err(|error| BlitzError::Window(error.to_string()))
    }
}

struct GpuPresenter {
    // pixels owns GpuWindowHandle, which contains copied OS handles. run_window declares presenter
    // after window, so Rust drops presenter before window and the captured handles remain valid.
    pixels: Pixels<'static>,
    width: usize,
    height: usize,
}

impl GpuPresenter {
    fn new(window: &Window, width: usize, height: usize) -> Result<Self> {
        let width = width.max(1);
        let height = height.max(1);
        let handle = GpuWindowHandle::capture(window)?;
        let surface = SurfaceTexture::new(width as u32, height as u32, handle);
        let pixels = PixelsBuilder::new(width as u32, height as u32, surface)
            .request_adapter_options(pixels::wgpu::RequestAdapterOptions {
                power_preference: pixels::wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
            })
            .device_descriptor(pixels::wgpu::DeviceDescriptor {
                label: Some("blitzpad-gpu-presenter"),
                required_limits: pixels::wgpu::Limits::downlevel_defaults(),
                ..Default::default()
            })
            .enable_vsync(true)
            .build()
            .map_err(|error| BlitzError::Window(format!("GPU presenter unavailable: {error}")))?;

        Ok(Self {
            pixels,
            width,
            height,
        })
    }

    fn present(&mut self, frame: &RenderFrame) -> Result<()> {
        self.resize(frame.width.max(1), frame.height.max(1))?;
        copy_frame_to_rgba(frame, self.pixels.frame_mut())?;
        self.pixels
            .render()
            .map_err(|error| BlitzError::Window(format!("GPU present failed: {error}")))
    }

    fn resize(&mut self, width: usize, height: usize) -> Result<()> {
        if self.width == width && self.height == height {
            return Ok(());
        }

        let width_u32 = width as u32;
        let height_u32 = height as u32;
        self.pixels
            .resize_surface(width_u32, height_u32)
            .map_err(|error| BlitzError::Window(format!("GPU surface resize failed: {error}")))?;
        self.pixels
            .resize_buffer(width_u32, height_u32)
            .map_err(|error| BlitzError::Window(format!("GPU buffer resize failed: {error}")))?;
        self.width = width;
        self.height = height;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct GpuWindowHandle {
    display: RawDisplayHandle,
    window: RawWindowHandle,
}

// Raw window/display handles are pointer-sized identifiers that wgpu may move across helper threads.
unsafe impl Send for GpuWindowHandle {}
// They are immutable copies, and GpuPresenter's drop-before-Window invariant keeps them live.
unsafe impl Sync for GpuWindowHandle {}

impl GpuWindowHandle {
    #[cfg(target_os = "windows")]
    fn capture(window: &Window) -> Result<Self> {
        // minifb 0.27's Windows HasWindowHandle implementation dereferences HWND incorrectly here,
        // so use the public native handle directly and wrap the pointer value for raw-window-handle.
        let hwnd = NonZeroIsize::new(window.get_window_handle() as isize)
            .ok_or_else(|| BlitzError::Window("window handle unavailable".to_owned()))?;
        let display = RawDisplayHandle::Windows(WindowsDisplayHandle::new());
        let window = RawWindowHandle::Win32(Win32WindowHandle::new(hwnd));
        Ok(Self { display, window })
    }

    #[cfg(not(target_os = "windows"))]
    fn capture(window: &Window) -> Result<Self> {
        let display = window
            .display_handle()
            .map_err(|error| BlitzError::Window(format!("display handle unavailable: {error}")))?
            .as_raw();
        let window = window
            .window_handle()
            .map_err(|error| BlitzError::Window(format!("window handle unavailable: {error}")))?
            .as_raw();
        Ok(Self { display, window })
    }
}

impl HasDisplayHandle for GpuWindowHandle {
    fn display_handle(&self) -> std::result::Result<DisplayHandle<'_>, HandleError> {
        // The raw handles were captured from the live minifb window and this presenter is dropped first.
        Ok(unsafe { DisplayHandle::borrow_raw(self.display) })
    }
}

impl HasWindowHandle for GpuWindowHandle {
    fn window_handle(&self) -> std::result::Result<WindowHandle<'_>, HandleError> {
        // The raw handles were captured from the live minifb window and this presenter is dropped first.
        Ok(unsafe { WindowHandle::borrow_raw(self.window) })
    }
}

fn copy_frame_to_rgba(frame: &RenderFrame, output: &mut [u8]) -> Result<()> {
    let expected_len = frame
        .width
        .checked_mul(frame.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .unwrap_or(usize::MAX);
    if output.len() != expected_len || frame.pixels.len().saturating_mul(4) != expected_len {
        return Err(BlitzError::Window(format!(
            "GPU frame upload size mismatch: frame={}x{} pixels={} rgba={}",
            frame.width,
            frame.height,
            frame.pixels.len(),
            output.len()
        )));
    }

    for (pixel, rgba) in frame.pixels.iter().zip(output.chunks_exact_mut(4)) {
        rgba[0] = ((pixel >> 16) & 0xff) as u8;
        rgba[1] = ((pixel >> 8) & 0xff) as u8;
        rgba[2] = (pixel & 0xff) as u8;
        rgba[3] = 0xff;
    }
    Ok(())
}

struct FindWindowState {
    window: Window,
    input_queue: Rc<RefCell<Vec<char>>>,
    query: String,
    match_case: bool,
    wrap_around: bool,
    forward: bool,
    mouse_was_down: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FindWindowView {
    query: String,
    match_case: bool,
    wrap_around: bool,
    forward: bool,
}

impl FindWindowState {
    fn view(&self) -> FindWindowView {
        FindWindowView {
            query: self.query.clone(),
            match_case: self.match_case,
            wrap_around: self.wrap_around,
            forward: self.forward,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScrollDrag {
    Vertical { grab_offset: usize },
    Horizontal { grab_offset: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Rect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

const FIND_FIELD_RECT: Rect = Rect {
    x: 118,
    y: 20,
    width: 205,
    height: 24,
};
const FIND_NEXT_BUTTON_RECT: Rect = Rect {
    x: 333,
    y: 20,
    width: 82,
    height: 24,
};
const FIND_CANCEL_BUTTON_RECT: Rect = Rect {
    x: 333,
    y: 52,
    width: 82,
    height: 24,
};
const FIND_MATCH_CASE_RECT: Rect = Rect {
    x: 16,
    y: 82,
    width: 130,
    height: 22,
};
const FIND_WRAP_RECT: Rect = Rect {
    x: 16,
    y: 110,
    width: 140,
    height: 22,
};
const FIND_UP_RADIO_RECT: Rect = Rect {
    x: 192,
    y: 100,
    width: 48,
    height: 20,
};
const FIND_DOWN_RADIO_RECT: Rect = Rect {
    x: 244,
    y: 100,
    width: 62,
    height: 20,
};
const FIND_DIRECTION_GROUP_RECT: Rect = Rect {
    x: 172,
    y: 80,
    width: 140,
    height: 50,
};
const GO_TO_FIELD_RECT: Rect = Rect {
    x: 10,
    y: 29,
    width: 228,
    height: 23,
};
const GO_TO_ACCEPT_BUTTON_RECT: Rect = Rect {
    x: 84,
    y: 65,
    width: 73,
    height: 23,
};
const GO_TO_CANCEL_BUTTON_RECT: Rect = Rect {
    x: 165,
    y: 65,
    width: 73,
    height: 23,
};

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrameSignature {
    width: usize,
    height: usize,
    ui_state: NotepadUiState,
    document_line_count: usize,
    active_menu: Option<usize>,
    status_text: Option<String>,
    first_visible_line: usize,
    horizontal_offset: usize,
    selected_range: Option<std::ops::Range<usize>>,
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
        status_text: state
            .status_bar_visible
            .then(|| gui_state.status_text().map(str::to_owned))
            .flatten(),
        first_visible_line: gui_state.first_visible_line,
        horizontal_offset: gui_state.horizontal_offset,
        selected_range: app.selected_range(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SearchSpec {
    query: String,
    match_case: bool,
    wrap_around: bool,
}

struct DialogWindowState {
    window: Window,
    input_queue: Rc<RefCell<Vec<char>>>,
    state: DialogState,
    mouse_was_down: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DialogState {
    Replace {
        query: String,
        replacement: String,
        active_field: ReplaceField,
        match_case: bool,
    },
    GoTo {
        line: String,
        replace_on_input: bool,
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
    pending_high_surrogate: Option<u16>,
}

impl TextInput {
    fn new(queue: Rc<RefCell<Vec<char>>>) -> Self {
        Self {
            queue,
            pending_high_surrogate: None,
        }
    }
}

impl InputCallback for TextInput {
    fn add_char(&mut self, uni_char: u32) {
        if let Some(character) = decode_text_input_char(uni_char, &mut self.pending_high_surrogate)
        {
            self.queue.borrow_mut().push(character);
        }
    }
}

fn decode_text_input_char(uni_char: u32, pending_high_surrogate: &mut Option<u16>) -> Option<char> {
    match uni_char {
        0xD800..=0xDBFF => {
            *pending_high_surrogate = Some(uni_char as u16);
            None
        }
        0xDC00..=0xDFFF => {
            let high = pending_high_surrogate.take()? as u32;
            let low = uni_char;
            char::from_u32(0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00))
        }
        _ => {
            *pending_high_surrogate = None;
            char::from_u32(normalize_legacy_pua_emoji(uni_char))
        }
    }
}

fn normalize_legacy_pua_emoji(uni_char: u32) -> u32 {
    // Some Windows text paths can surface old BMP-private emoji code units;
    // map the emoticon block back to the standard supplementary-plane range.
    if (0xF600..=0xF64F).contains(&uni_char) {
        uni_char + 0x10000
    } else {
        uni_char
    }
}

fn handle_mouse(window: &Window, app: &mut BlitzApp, gui_state: &mut GuiState) -> Result<bool> {
    let mouse_down = window.get_mouse_down(MouseButton::Left);
    let clicked = mouse_down && !gui_state.mouse_was_down;
    gui_state.mouse_was_down = mouse_down;
    if !mouse_down {
        gui_state.scroll_drag = None;
        gui_state.mouse_selecting = false;
        if app.selected_range().is_none() {
            gui_state.selection_anchor = None;
            gui_state.selection_focus = None;
        }
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

    if gui_state.mouse_selecting {
        update_mouse_selection(app, gui_state, width, height, x, y)?;
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
        if is_shift_down(window) {
            let anchor = gui_state.selection_anchor.unwrap_or(app.caret_offset());
            app.set_selection_range(anchor, offset)?;
            gui_state.selection_anchor = Some(anchor);
            gui_state.selection_focus = Some(offset);
        } else {
            app.set_caret_offset(offset)?;
            gui_state.selection_anchor = Some(offset);
            gui_state.selection_focus = Some(offset);
        }
        gui_state.mouse_selecting = true;
    }
    Ok(false)
}

fn update_mouse_selection(
    app: &mut BlitzApp,
    gui_state: &mut GuiState,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
) -> Result<()> {
    let Some(anchor) = gui_state.selection_anchor else {
        return Ok(());
    };
    let Some(focus) = text_offset_for_point(app, gui_state, width, height, x, y) else {
        return Ok(());
    };
    if gui_state.selection_focus == Some(focus) {
        return Ok(());
    }
    gui_state.selection_focus = Some(focus);
    app.set_selection_range(anchor, focus)
}

fn handle_scrollbar_click(
    app: &BlitzApp,
    gui_state: &mut GuiState,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
) -> Result<bool> {
    let Some(metrics) = editor_metrics_for_app(app, width, height) else {
        return Ok(false);
    };

    if x >= metrics.text_width && y >= metrics.text_top && y < metrics.editor_bottom {
        let content_len = vertical_content_len(app, gui_state, metrics);
        let (thumb_y, thumb_height) = scroll_thumb(
            metrics.text_top,
            metrics.text_height,
            content_len,
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
        gui_state.scroll_vertical(app, delta, metrics);
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
    let Some(metrics) = editor_metrics_for_app(app, width, height) else {
        return Ok(());
    };

    match gui_state.scroll_drag {
        Some(ScrollDrag::Vertical { grab_offset }) => {
            let content_len = vertical_content_len(app, gui_state, metrics);
            let (_, thumb_height) = scroll_thumb(
                metrics.text_top,
                metrics.text_height,
                content_len,
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
                content_len,
                metrics.visible_line_count,
                thumb_y,
            );
            gui_state.scroll_vertical(app, 0, metrics);
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

fn open_find_window(app: &BlitzApp, gui_state: &mut GuiState) -> Result<()> {
    if gui_state.find_window.is_some() {
        return Ok(());
    }

    let input_queue = Rc::new(RefCell::new(Vec::new()));
    let window = Window::new(
        "Find",
        FIND_WINDOW_WIDTH,
        FIND_WINDOW_HEIGHT,
        WindowOptions {
            resize: false,
            ..WindowOptions::default()
        },
    )
    .map_err(|error| BlitzError::Window(error.to_string()))?;

    let query = app
        .selected_text()
        .or_else(|| {
            gui_state
                .last_search
                .as_ref()
                .map(|search| search.query.clone())
        })
        .unwrap_or_default();
    let (match_case, wrap_around) = gui_state
        .last_search
        .as_ref()
        .map(|search| (search.match_case, search.wrap_around))
        .unwrap_or((false, false));

    gui_state.find_window = Some(FindWindowState {
        window,
        input_queue: Rc::clone(&input_queue),
        query,
        match_case,
        wrap_around,
        forward: true,
        mouse_was_down: false,
    });
    if let Some(find_window) = gui_state.find_window.as_mut() {
        find_window
            .window
            .set_input_callback(Box::new(TextInput::new(input_queue)));
    }
    Ok(())
}

fn handle_find_window(app: &mut BlitzApp, gui_state: &mut GuiState) -> Result<()> {
    let Some(mut find_window) = gui_state.find_window.as_mut() else {
        return Ok(());
    };

    position_find_ime_window(find_window, &gui_state.fonts);
    let frame = render_find_window(&find_window, &gui_state.fonts);
    find_window
        .window
        .update_with_buffer(&frame.pixels, frame.width, frame.height)
        .map_err(|error| BlitzError::Window(error.to_string()))?;

    let mut keep_open = find_window.window.is_open();
    if keep_open {
        keep_open = handle_find_window_keys(
            app,
            &mut gui_state.last_search,
            &mut gui_state.pending_reveal_selection,
            &mut gui_state.clipboard,
            &mut find_window,
        );
    }
    if keep_open {
        keep_open = handle_find_window_mouse(
            app,
            &mut gui_state.last_search,
            &mut gui_state.pending_reveal_selection,
            &mut find_window,
        );
    }
    if keep_open {
        position_find_ime_window(find_window, &gui_state.fonts);
        let frame = render_find_window(&find_window, &gui_state.fonts);
        find_window
            .window
            .update_with_buffer(&frame.pixels, frame.width, frame.height)
            .map_err(|error| BlitzError::Window(error.to_string()))?;
    } else {
        gui_state.find_window = None;
    }
    Ok(())
}

fn handle_find_window_keys(
    app: &mut BlitzApp,
    last_search: &mut Option<SearchSpec>,
    pending_reveal_selection: &mut bool,
    clipboard: &mut String,
    find_window: &mut FindWindowState,
) -> bool {
    let command_down = is_command_down(&find_window.window);
    if find_window
        .window
        .is_key_pressed(Key::Escape, KeyRepeat::No)
    {
        return false;
    }
    if find_window.window.is_key_pressed(Key::Enter, KeyRepeat::No)
        || find_window
            .window
            .is_key_pressed(Key::NumPadEnter, KeyRepeat::No)
    {
        run_find_from_window(app, last_search, pending_reveal_selection, find_window);
    }
    if find_window
        .window
        .is_key_pressed(Key::Backspace, KeyRepeat::Yes)
    {
        find_window.query.pop();
    }
    if find_window.window.is_key_pressed(Key::Up, KeyRepeat::No) {
        find_window.forward = false;
    }
    if find_window.window.is_key_pressed(Key::Down, KeyRepeat::No) {
        find_window.forward = true;
    }
    if command_down && find_window.window.is_key_pressed(Key::V, KeyRepeat::No) {
        paste_into_find_query(&mut find_window.query, clipboard);
        find_window.input_queue.borrow_mut().clear();
        return true;
    }

    let characters = find_window
        .input_queue
        .borrow_mut()
        .drain(..)
        .collect::<Vec<_>>();
    if should_use_find_ascii_fallback(
        characters.is_empty(),
        command_down,
        ime_is_open(&find_window.window),
    ) {
        for key in find_window.window.get_keys_pressed(KeyRepeat::Yes) {
            if let Some(character) =
                ascii_find_char_for_key(key, is_shift_down(&find_window.window))
            {
                find_window.query.push(character);
            }
        }
    } else {
        for character in characters {
            if !character.is_control() {
                find_window.query.push(character);
            }
        }
    }
    true
}

fn should_use_find_ascii_fallback(
    characters_empty: bool,
    command_down: bool,
    ime_open: bool,
) -> bool {
    characters_empty && !command_down && !ime_open
}

#[cfg(target_os = "windows")]
fn ime_is_open(window: &Window) -> bool {
    let Some(hwnd) = window_hwnd(window) else {
        return false;
    };

    unsafe {
        let context = ImmGetContext(hwnd);
        if context.is_null() {
            return false;
        }
        let open = ImmGetOpenStatus(context) != 0;
        ImmReleaseContext(hwnd, context);
        open
    }
}

#[cfg(not(target_os = "windows"))]
fn ime_is_open(_window: &Window) -> bool {
    false
}

#[cfg(target_os = "windows")]
fn position_find_ime_window(find_window: &FindWindowState, fonts: &FontStack) {
    let x = find_ime_caret_x(fonts, &find_window.query);
    position_ime_composition_window(&find_window.window, x, find_ime_caret_y());
}

#[cfg(not(target_os = "windows"))]
fn position_find_ime_window(_find_window: &FindWindowState, _fonts: &FontStack) {}

fn find_ime_caret_x(fonts: &FontStack, query: &str) -> usize {
    let visible = text_prefix_for_width(
        fonts,
        query,
        TextRole::Find,
        FIND_FIELD_RECT.width.saturating_sub(12),
    );
    (FIND_FIELD_RECT.x + 6 + fonts.measure(visible, TextRole::Find))
        .min(FIND_FIELD_RECT.x + FIND_FIELD_RECT.width - 4)
}

fn find_ime_caret_y() -> usize {
    centered_text_y(FIND_FIELD_RECT, TextRole::Find)
}

fn update_editor_ime_state(
    window: &Window,
    app: &BlitzApp,
    gui_state: &mut GuiState,
    width: usize,
    height: usize,
) {
    if gui_state.find_window.is_some() || gui_state.dialog.is_some() {
        gui_state.editor_ime_composition.clear();
        return;
    }
    gui_state.editor_ime_composition = ime_composition_text(window).unwrap_or_default();
    if let Some((x, y)) = editor_ime_caret_point(app, gui_state, width, height) {
        position_ime_composition_window(window, x, y);
    }
}

fn editor_ime_caret_point(
    app: &BlitzApp,
    gui_state: &GuiState,
    width: usize,
    height: usize,
) -> Option<(usize, usize)> {
    let metrics = editor_metrics_for_app(app, width, height)?;
    let first_visible_line = gui_state
        .first_visible_line
        .min(max_first_visible_line_for_metrics(app, gui_state, metrics));
    let visible_lines = editor_visible_lines(
        app,
        first_visible_line,
        metrics,
        gui_state,
        app.settings().word_wrap,
    );
    if visible_lines.is_empty() {
        return Some((
            TEXT_MARGIN_X,
            editor_ime_y(metrics.text_top + TEXT_MARGIN_Y),
        ));
    }

    let caret_line = app.document().line_for_offset(app.caret_offset()).ok()? + 1;
    let visible_index =
        visible_line_index_for_caret(&visible_lines, caret_line, app.caret_offset())?;
    let line = &visible_lines[visible_index];
    let local_offset = app
        .caret_offset()
        .saturating_sub(line.byte_range.start)
        .min(line.text.len());
    let prefix = line.text.get(..local_offset).unwrap_or(&line.text);
    let visible_prefix = text_prefix_for_width(
        &gui_state.fonts,
        prefix,
        TextRole::Editor,
        metrics.editor_text_width,
    );
    let x = (TEXT_MARGIN_X + gui_state.fonts.measure(visible_prefix, TextRole::Editor))
        .min(TEXT_MARGIN_X + metrics.editor_text_width);
    let y = editor_ime_y(metrics.text_top + TEXT_MARGIN_Y + visible_index * EDITOR_LINE_HEIGHT);
    Some((x, y))
}

fn editor_ime_y(line_top: usize) -> usize {
    line_top + EDITOR_BASELINE_OFFSET
}

#[cfg(target_os = "windows")]
fn ime_composition_text(window: &Window) -> Option<String> {
    let hwnd = window_hwnd(window)?;
    unsafe {
        let context = ImmGetContext(hwnd);
        if context.is_null() {
            return None;
        }

        let byte_len = ImmGetCompositionStringW(context, GCS_COMPSTR, core::ptr::null_mut(), 0);
        let text = if byte_len > 0 {
            let mut buffer = vec![0u16; (byte_len as usize).div_ceil(2)];
            let read_len = ImmGetCompositionStringW(
                context,
                GCS_COMPSTR,
                buffer.as_mut_ptr().cast(),
                byte_len as u32,
            );
            if read_len > 0 {
                Some(String::from_utf16_lossy(&buffer[..read_len as usize / 2]))
            } else {
                None
            }
        } else {
            None
        };
        ImmReleaseContext(hwnd, context);
        text.filter(|text| !text.is_empty())
    }
}

#[cfg(not(target_os = "windows"))]
fn ime_composition_text(_window: &Window) -> Option<String> {
    None
}

#[cfg(target_os = "windows")]
fn position_ime_composition_window(window: &Window, x: usize, y: usize) {
    let Some(hwnd) = window_hwnd(window) else {
        return;
    };

    unsafe {
        let context = ImmGetContext(hwnd);
        if context.is_null() {
            return;
        }
        let form = COMPOSITIONFORM {
            dwStyle: CFS_FORCE_POSITION,
            ptCurrentPos: POINT {
                x: x as i32,
                y: y as i32,
            },
            rcArea: RECT::default(),
        };
        ImmSetCompositionWindow(context, &form);
        ImmReleaseContext(hwnd, context);
    }
}

#[cfg(not(target_os = "windows"))]
fn position_ime_composition_window(_window: &Window, _x: usize, _y: usize) {}

#[cfg(target_os = "windows")]
fn window_hwnd(window: &Window) -> Option<HWND> {
    let hwnd = window.get_window_handle();
    (!hwnd.is_null()).then_some(hwnd as HWND)
}

fn ascii_find_char_for_key(key: Key, shift: bool) -> Option<char> {
    match key {
        Key::A => Some(if shift { 'A' } else { 'a' }),
        Key::B => Some(if shift { 'B' } else { 'b' }),
        Key::C => Some(if shift { 'C' } else { 'c' }),
        Key::D => Some(if shift { 'D' } else { 'd' }),
        Key::E => Some(if shift { 'E' } else { 'e' }),
        Key::F => Some(if shift { 'F' } else { 'f' }),
        Key::G => Some(if shift { 'G' } else { 'g' }),
        Key::H => Some(if shift { 'H' } else { 'h' }),
        Key::I => Some(if shift { 'I' } else { 'i' }),
        Key::J => Some(if shift { 'J' } else { 'j' }),
        Key::K => Some(if shift { 'K' } else { 'k' }),
        Key::L => Some(if shift { 'L' } else { 'l' }),
        Key::M => Some(if shift { 'M' } else { 'm' }),
        Key::N => Some(if shift { 'N' } else { 'n' }),
        Key::O => Some(if shift { 'O' } else { 'o' }),
        Key::P => Some(if shift { 'P' } else { 'p' }),
        Key::Q => Some(if shift { 'Q' } else { 'q' }),
        Key::R => Some(if shift { 'R' } else { 'r' }),
        Key::S => Some(if shift { 'S' } else { 's' }),
        Key::T => Some(if shift { 'T' } else { 't' }),
        Key::U => Some(if shift { 'U' } else { 'u' }),
        Key::V => Some(if shift { 'V' } else { 'v' }),
        Key::W => Some(if shift { 'W' } else { 'w' }),
        Key::X => Some(if shift { 'X' } else { 'x' }),
        Key::Y => Some(if shift { 'Y' } else { 'y' }),
        Key::Z => Some(if shift { 'Z' } else { 'z' }),
        Key::Key0 => Some(if shift { ')' } else { '0' }),
        Key::Key1 => Some(if shift { '!' } else { '1' }),
        Key::Key2 => Some(if shift { '@' } else { '2' }),
        Key::Key3 => Some(if shift { '#' } else { '3' }),
        Key::Key4 => Some(if shift { '$' } else { '4' }),
        Key::Key5 => Some(if shift { '%' } else { '5' }),
        Key::Key6 => Some(if shift { '^' } else { '6' }),
        Key::Key7 => Some(if shift { '&' } else { '7' }),
        Key::Key8 => Some(if shift { '*' } else { '8' }),
        Key::Key9 => Some(if shift { '(' } else { '9' }),
        Key::Space => Some(' '),
        Key::Minus => Some(if shift { '_' } else { '-' }),
        Key::Equal => Some(if shift { '+' } else { '=' }),
        Key::Comma => Some(if shift { '<' } else { ',' }),
        Key::Period => Some(if shift { '>' } else { '.' }),
        Key::Slash => Some(if shift { '?' } else { '/' }),
        Key::Semicolon => Some(if shift { ':' } else { ';' }),
        Key::Apostrophe => Some(if shift { '"' } else { '\'' }),
        Key::LeftBracket => Some(if shift { '{' } else { '[' }),
        Key::RightBracket => Some(if shift { '}' } else { ']' }),
        Key::Backslash => Some(if shift { '|' } else { '\\' }),
        Key::Backquote => Some(if shift { '~' } else { '`' }),
        Key::NumPad0 => Some('0'),
        Key::NumPad1 => Some('1'),
        Key::NumPad2 => Some('2'),
        Key::NumPad3 => Some('3'),
        Key::NumPad4 => Some('4'),
        Key::NumPad5 => Some('5'),
        Key::NumPad6 => Some('6'),
        Key::NumPad7 => Some('7'),
        Key::NumPad8 => Some('8'),
        Key::NumPad9 => Some('9'),
        Key::NumPadDot => Some('.'),
        Key::NumPadSlash => Some('/'),
        Key::NumPadAsterisk => Some('*'),
        Key::NumPadMinus => Some('-'),
        Key::NumPadPlus => Some('+'),
        _ => None,
    }
}

fn handle_find_window_mouse(
    app: &mut BlitzApp,
    last_search: &mut Option<SearchSpec>,
    pending_reveal_selection: &mut bool,
    find_window: &mut FindWindowState,
) -> bool {
    let mouse_down = find_window.window.get_mouse_down(MouseButton::Left);
    let clicked = mouse_down && !find_window.mouse_was_down;
    find_window.mouse_was_down = mouse_down;
    if !clicked {
        return true;
    }

    let Some((mouse_x, mouse_y)) = find_window.window.get_mouse_pos(MouseMode::Discard) else {
        return true;
    };
    let x = mouse_x as usize;
    let y = mouse_y as usize;

    if hit_rect(x, y, FIND_NEXT_BUTTON_RECT) {
        if !find_window.query.is_empty() {
            run_find_from_window(app, last_search, pending_reveal_selection, find_window);
        }
    } else if hit_rect(x, y, FIND_CANCEL_BUTTON_RECT) {
        return false;
    } else if hit_rect(x, y, FIND_MATCH_CASE_RECT) {
        find_window.match_case = !find_window.match_case;
    } else if hit_rect(x, y, FIND_WRAP_RECT) {
        find_window.wrap_around = !find_window.wrap_around;
    } else if hit_rect(x, y, FIND_UP_RADIO_RECT) {
        find_window.forward = false;
    } else if hit_rect(x, y, FIND_DOWN_RADIO_RECT) {
        find_window.forward = true;
    }

    true
}

fn run_find_from_window(
    app: &mut BlitzApp,
    last_search: &mut Option<SearchSpec>,
    pending_reveal_selection: &mut bool,
    find_window: &FindWindowState,
) {
    if find_window.query.is_empty() {
        return;
    }

    if app
        .find_text_with_options(
            &find_window.query,
            find_window.match_case,
            find_window.forward,
            find_window.wrap_around,
        )
        .unwrap_or(false)
    {
        *pending_reveal_selection = true;
        *last_search = Some(SearchSpec {
            query: find_window.query.clone(),
            match_case: find_window.match_case,
            wrap_around: find_window.wrap_around,
        });
    }
}

fn hit_rect(x: usize, y: usize, rect: Rect) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

fn render_find_window(find_window: &FindWindowState, fonts: &FontStack) -> RenderFrame {
    render_find_window_view(&find_window.view(), fonts)
}

fn open_dialog_window(gui_state: &mut GuiState, state: DialogState) -> Result<()> {
    if should_defer_dialog_window(&state, gui_state.dialog.is_some()) {
        gui_state.pending_info_dialog = Some(state);
        return Ok(());
    }

    let title = dialog_window_title(&state).to_owned();
    let (width, height) = dialog_window_size(&state);
    let input_queue = Rc::new(RefCell::new(Vec::new()));
    let window = Window::new(
        &title,
        width,
        height,
        WindowOptions {
            resize: false,
            ..WindowOptions::default()
        },
    )
    .map_err(|error| BlitzError::Window(error.to_string()))?;

    gui_state.dialog = Some(DialogWindowState {
        window,
        input_queue: Rc::clone(&input_queue),
        state,
        mouse_was_down: false,
    });
    if let Some(dialog) = gui_state.dialog.as_mut() {
        dialog
            .window
            .set_input_callback(Box::new(TextInput::new(input_queue)));
    }
    Ok(())
}

fn should_defer_dialog_window(state: &DialogState, dialog_open: bool) -> bool {
    dialog_open && matches!(state, DialogState::Info { .. })
}

fn open_info_window(gui_state: &mut GuiState, title: &str, message: &str) -> Result<()> {
    open_dialog_window(
        gui_state,
        DialogState::Info {
            title: title.to_owned(),
            message: message.to_owned(),
        },
    )
}

fn dialog_window_title(state: &DialogState) -> &'static str {
    match state {
        DialogState::Replace { .. } => "Replace",
        DialogState::GoTo { .. } => "Go To Line",
        DialogState::Info { title, .. } => match title.as_str() {
            "Print" => "Print",
            "Font" => "Font",
            "About Notepad" => "About Notepad",
            "Save" => "Save",
            _ => "Notepad",
        },
    }
}

fn dialog_window_size(state: &DialogState) -> (usize, usize) {
    match state {
        DialogState::Replace { .. } => (470, 220),
        DialogState::GoTo { .. } => (GO_TO_WINDOW_WIDTH, GO_TO_WINDOW_HEIGHT),
        DialogState::Info { .. } => (470, 210),
    }
}

fn handle_dialog_window(app: &mut BlitzApp, gui_state: &mut GuiState) -> Result<()> {
    let Some(mut dialog) = gui_state.dialog.take() else {
        return Ok(());
    };

    let frame = render_dialog_window(&dialog.state, &gui_state.fonts);
    dialog
        .window
        .update_with_buffer(&frame.pixels, frame.width, frame.height)
        .map_err(|error| BlitzError::Window(error.to_string()))?;

    let mut keep_open = dialog.window.is_open();
    if keep_open {
        keep_open = handle_dialog_window_keys(
            app,
            &mut gui_state.last_search,
            &mut gui_state.pending_reveal_selection,
            &mut gui_state.clipboard,
            &mut dialog,
        )?;
    }
    if keep_open {
        handle_dialog_window_text_input(&mut dialog);
    }
    if keep_open {
        keep_open = handle_dialog_window_mouse(
            app,
            &mut gui_state.last_search,
            &mut gui_state.pending_reveal_selection,
            &mut dialog,
        )?;
    }

    if keep_open {
        let frame = render_dialog_window(&dialog.state, &gui_state.fonts);
        dialog
            .window
            .update_with_buffer(&frame.pixels, frame.width, frame.height)
            .map_err(|error| BlitzError::Window(error.to_string()))?;
        gui_state.dialog = Some(dialog);
    } else if let Some(pending) = gui_state.pending_info_dialog.take() {
        open_dialog_window(gui_state, pending)?;
    }

    Ok(())
}

fn handle_dialog_window_keys(
    app: &mut BlitzApp,
    last_search: &mut Option<SearchSpec>,
    pending_reveal_selection: &mut bool,
    clipboard: &mut String,
    dialog: &mut DialogWindowState,
) -> Result<bool> {
    if dialog.window.is_key_pressed(Key::Escape, KeyRepeat::No) {
        return Ok(false);
    }
    if is_command_down(&dialog.window) && dialog.window.is_key_pressed(Key::V, KeyRepeat::No) {
        paste_into_dialog_field(&mut dialog.state, clipboard);
        dialog.input_queue.borrow_mut().clear();
        return Ok(true);
    }
    if dialog.window.is_key_pressed(Key::Backspace, KeyRepeat::Yes) {
        backspace_dialog_field(&mut dialog.state);
    }
    if dialog.window.is_key_pressed(Key::Tab, KeyRepeat::No) {
        tab_dialog_field(&mut dialog.state);
    }
    if dialog.window.is_key_pressed(Key::Space, KeyRepeat::No) {
        toggle_dialog_option(&mut dialog.state);
    }
    if dialog.window.is_key_pressed(Key::Enter, KeyRepeat::No)
        || dialog
            .window
            .is_key_pressed(Key::NumPadEnter, KeyRepeat::No)
    {
        if matches!(&dialog.state, DialogState::GoTo { .. }) {
            return activate_go_to_button(
                app,
                last_search,
                pending_reveal_selection,
                &mut dialog.state,
            );
        }
        return accept_dialog_state(
            app,
            last_search,
            pending_reveal_selection,
            &mut dialog.state,
            is_command_down(&dialog.window),
        );
    }
    Ok(true)
}

fn handle_dialog_window_text_input(dialog: &mut DialogWindowState) {
    let characters = dialog
        .input_queue
        .borrow_mut()
        .drain(..)
        .collect::<Vec<_>>();
    for character in characters {
        if !character.is_control() {
            append_dialog_character(&mut dialog.state, character);
        }
    }
}

fn handle_dialog_window_mouse(
    app: &mut BlitzApp,
    last_search: &mut Option<SearchSpec>,
    pending_reveal_selection: &mut bool,
    dialog: &mut DialogWindowState,
) -> Result<bool> {
    let mouse_down = dialog.window.get_mouse_down(MouseButton::Left);
    let clicked = mouse_down && !dialog.mouse_was_down;
    dialog.mouse_was_down = mouse_down;
    if !clicked {
        return Ok(true);
    }

    let Some((mouse_x, mouse_y)) = dialog.window.get_mouse_pos(MouseMode::Discard) else {
        return Ok(true);
    };
    let x = mouse_x as usize;
    let y = mouse_y as usize;

    if !matches!(&dialog.state, DialogState::GoTo { .. }) {
        return Ok(true);
    }

    if hit_rect(x, y, GO_TO_ACCEPT_BUTTON_RECT) {
        activate_go_to_button(
            app,
            last_search,
            pending_reveal_selection,
            &mut dialog.state,
        )
    } else if hit_rect(x, y, GO_TO_CANCEL_BUTTON_RECT) {
        Ok(false)
    } else {
        Ok(true)
    }
}

fn activate_go_to_button(
    app: &mut BlitzApp,
    last_search: &mut Option<SearchSpec>,
    pending_reveal_selection: &mut bool,
    dialog: &mut DialogState,
) -> Result<bool> {
    accept_dialog_state(app, last_search, pending_reveal_selection, dialog, false)
}

fn render_dialog_window(dialog: &DialogState, fonts: &FontStack) -> RenderFrame {
    let (width, height) = dialog_window_size(dialog);
    let mut canvas = Canvas::new(width, height, COLOR_WINDOW);
    draw_dialog(&mut canvas, dialog, fonts);
    canvas.into_frame()
}

fn render_find_window_view(view: &FindWindowView, fonts: &FontStack) -> RenderFrame {
    let mut canvas = Canvas::new(FIND_WINDOW_WIDTH, FIND_WINDOW_HEIGHT, COLOR_WINDOW);
    canvas.text(
        16,
        centered_text_y(FIND_FIELD_RECT, TextRole::Find),
        "Find what:",
        COLOR_TEXT,
        TextRole::Find,
        fonts,
    );
    draw_find_text_field(&mut canvas, fonts, &view.query);
    draw_find_button(
        &mut canvas,
        fonts,
        FIND_NEXT_BUTTON_RECT,
        "Find Next",
        !view.query.is_empty(),
    );
    draw_find_button(&mut canvas, fonts, FIND_CANCEL_BUTTON_RECT, "Cancel", true);

    draw_checkbox(
        &mut canvas,
        fonts,
        FIND_MATCH_CASE_RECT,
        "Match case",
        view.match_case,
    );
    draw_checkbox(
        &mut canvas,
        fonts,
        FIND_WRAP_RECT,
        "Wrap around",
        view.wrap_around,
    );
    draw_group_box(&mut canvas, fonts, FIND_DIRECTION_GROUP_RECT, "Direction");
    draw_radio_button(&mut canvas, fonts, FIND_UP_RADIO_RECT, "Up", !view.forward);
    draw_radio_button(
        &mut canvas,
        fonts,
        FIND_DOWN_RADIO_RECT,
        "Down",
        view.forward,
    );
    canvas.into_frame()
}

fn draw_find_text_field(canvas: &mut Canvas, fonts: &FontStack, query: &str) {
    canvas.fill_rect(
        FIND_FIELD_RECT.x,
        FIND_FIELD_RECT.y,
        FIND_FIELD_RECT.width,
        FIND_FIELD_RECT.height,
        COLOR_TEXT_AREA,
    );
    canvas.rect(
        FIND_FIELD_RECT.x,
        FIND_FIELD_RECT.y,
        FIND_FIELD_RECT.width,
        FIND_FIELD_RECT.height,
        COLOR_FOCUS_BORDER,
    );
    let visible = text_prefix_for_width(
        fonts,
        query,
        TextRole::Find,
        FIND_FIELD_RECT.width.saturating_sub(12),
    );
    canvas.text(
        FIND_FIELD_RECT.x + 6,
        centered_text_y(FIND_FIELD_RECT, TextRole::Find),
        visible,
        COLOR_TEXT,
        TextRole::Find,
        fonts,
    );
    let caret_x = (FIND_FIELD_RECT.x + 6 + fonts.measure(visible, TextRole::Find))
        .min(FIND_FIELD_RECT.x + FIND_FIELD_RECT.width - 4);
    canvas.fill_rect(
        caret_x,
        FIND_FIELD_RECT.y + 4,
        2,
        FIND_FIELD_RECT.height - 8,
        COLOR_CARET,
    );
}

fn draw_go_to_dialog(canvas: &mut Canvas, fonts: &FontStack, line: &str, selected: bool) {
    canvas.text(10, 10, "Line number:", COLOR_TEXT, TextRole::Find, fonts);
    draw_go_to_text_field(canvas, fonts, line, selected);
    draw_go_to_button(canvas, fonts, GO_TO_ACCEPT_BUTTON_RECT, "Go To", true);
    draw_go_to_button(canvas, fonts, GO_TO_CANCEL_BUTTON_RECT, "Cancel", false);
}

fn draw_go_to_text_field(canvas: &mut Canvas, fonts: &FontStack, line: &str, selected: bool) {
    canvas.fill_rect(
        GO_TO_FIELD_RECT.x,
        GO_TO_FIELD_RECT.y,
        GO_TO_FIELD_RECT.width,
        GO_TO_FIELD_RECT.height,
        COLOR_TEXT_AREA,
    );
    canvas.rect(
        GO_TO_FIELD_RECT.x,
        GO_TO_FIELD_RECT.y,
        GO_TO_FIELD_RECT.width,
        GO_TO_FIELD_RECT.height,
        COLOR_FOCUS_BORDER,
    );
    let visible = text_prefix_for_width(
        fonts,
        line,
        TextRole::Find,
        GO_TO_FIELD_RECT.width.saturating_sub(12),
    );
    let text_x = GO_TO_FIELD_RECT.x + 5;
    let text_y = centered_text_y(GO_TO_FIELD_RECT, TextRole::Find);
    let text_color = if selected && !visible.is_empty() {
        let selection_width = fonts
            .measure(visible, TextRole::Find)
            .min(GO_TO_FIELD_RECT.width.saturating_sub(10));
        canvas.fill_rect(
            text_x,
            GO_TO_FIELD_RECT.y + 3,
            selection_width.max(1),
            GO_TO_FIELD_RECT.height - 6,
            COLOR_FOCUS_BORDER,
        );
        COLOR_SELECTED_TEXT
    } else {
        COLOR_TEXT
    };
    canvas.text(text_x, text_y, visible, text_color, TextRole::Find, fonts);
    if !selected {
        let caret_x = (text_x + fonts.measure(visible, TextRole::Find))
            .min(GO_TO_FIELD_RECT.x + GO_TO_FIELD_RECT.width - 4);
        canvas.fill_rect(
            caret_x,
            GO_TO_FIELD_RECT.y + 4,
            1,
            GO_TO_FIELD_RECT.height - 8,
            COLOR_CARET,
        );
    }
}

fn draw_go_to_button(
    canvas: &mut Canvas,
    fonts: &FontStack,
    rect: Rect,
    label: &str,
    default_button: bool,
) {
    if default_button {
        canvas.rect(
            rect.x - 1,
            rect.y - 1,
            rect.width + 2,
            rect.height + 2,
            COLOR_FOCUS_BORDER,
        );
    }
    canvas.fill_rect(rect.x, rect.y, rect.width, rect.height, COLOR_BUTTON);
    canvas.rect(rect.x, rect.y, rect.width, rect.height, COLOR_BORDER);
    let label = text_prefix_for_width(fonts, label, TextRole::Find, rect.width.saturating_sub(8));
    let text_x = rect.x
        + rect
            .width
            .saturating_sub(fonts.measure(label, TextRole::Find))
            / 2;
    canvas.text(
        text_x,
        centered_text_y(rect, TextRole::Find),
        label,
        COLOR_TEXT,
        TextRole::Find,
        fonts,
    );
}

fn draw_find_button(
    canvas: &mut Canvas,
    fonts: &FontStack,
    rect: Rect,
    label: &str,
    enabled: bool,
) {
    canvas.fill_rect(rect.x, rect.y, rect.width, rect.height, COLOR_BUTTON);
    canvas.rect(rect.x, rect.y, rect.width, rect.height, COLOR_BORDER);
    let text_color = if enabled {
        COLOR_TEXT
    } else {
        COLOR_DISABLED_TEXT
    };
    let label = text_prefix_for_width(fonts, label, TextRole::Find, rect.width.saturating_sub(8));
    let text_x = rect.x
        + rect
            .width
            .saturating_sub(fonts.measure(label, TextRole::Find))
            / 2;
    canvas.text(
        text_x,
        centered_text_y(rect, TextRole::Find),
        label,
        text_color,
        TextRole::Find,
        fonts,
    );
}

fn draw_checkbox(canvas: &mut Canvas, fonts: &FontStack, rect: Rect, label: &str, checked: bool) {
    let box_y = rect.y + (rect.height.saturating_sub(14)) / 2;
    canvas.fill_rect(rect.x, box_y, 14, 14, COLOR_TEXT_AREA);
    canvas.rect(rect.x, box_y, 14, 14, COLOR_TEXT);
    if checked {
        canvas.line(rect.x + 3, box_y + 7, rect.x + 6, box_y + 11, COLOR_TEXT);
        canvas.line(rect.x + 6, box_y + 11, rect.x + 12, box_y + 2, COLOR_TEXT);
    }
    canvas.text(
        rect.x + 22,
        centered_text_y(rect, TextRole::Find),
        label,
        COLOR_TEXT,
        TextRole::Find,
        fonts,
    );
}

fn draw_group_box(canvas: &mut Canvas, fonts: &FontStack, rect: Rect, label: &str) {
    canvas.rect(rect.x, rect.y, rect.width, rect.height, COLOR_GROUP_BOX);
    canvas.fill_rect(
        rect.x + 12,
        rect.y,
        fonts.measure(label, TextRole::Find) + 8,
        14,
        COLOR_WINDOW,
    );
    canvas.text(
        rect.x + 16,
        rect.y - 2,
        label,
        COLOR_TEXT,
        TextRole::Find,
        fonts,
    );
}

fn draw_radio_button(
    canvas: &mut Canvas,
    fonts: &FontStack,
    rect: Rect,
    label: &str,
    checked: bool,
) {
    let cx = rect.x + 8;
    let cy = rect.y + rect.height / 2;
    draw_circle(canvas, cx as i32, cy as i32, 7, COLOR_TEXT);
    if checked {
        canvas.fill_rect(cx - 3, cy - 3, 6, 6, COLOR_TEXT);
    }
    canvas.text(
        rect.x + 20,
        centered_text_y(rect, TextRole::Find),
        label,
        COLOR_TEXT,
        TextRole::Find,
        fonts,
    );
}

fn centered_text_y(rect: Rect, role: TextRole) -> usize {
    let text_height = match role {
        TextRole::Find => 16,
        TextRole::Ui => 18,
        TextRole::Editor => EDITOR_LINE_HEIGHT,
    };
    rect.y + rect.height.saturating_sub(text_height) / 2
}

fn draw_circle(canvas: &mut Canvas, center_x: i32, center_y: i32, radius: i32, color: u32) {
    for y in -radius..=radius {
        for x in -radius..=radius {
            let distance = x * x + y * y;
            if distance >= radius * radius - radius && distance <= radius * radius + radius {
                canvas.set_pixel(center_x + x, center_y + y, color);
            }
        }
    }
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
    let Some(metrics) = editor_metrics_for_app(app, width, height) else {
        return Ok(());
    };

    if !is_shift_down(window) {
        let delta_lines = (-(scroll_y * WHEEL_LINES as f32).round()) as isize;
        if delta_lines != 0 {
            gui_state.scroll_vertical(app, delta_lines, metrics);
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
    if let Some(metrics) = editor_metrics_for_app(app, width.max(MIN_WIDTH), height.max(MIN_HEIGHT))
    {
        gui_state.scroll_vertical(
            app,
            direction * metrics.visible_line_count as isize,
            metrics,
        );
    }
    Ok(())
}

fn handle_keys(
    input_queue: &Rc<RefCell<Vec<char>>>,
    window: &Window,
    app: &mut BlitzApp,
    gui_state: &mut GuiState,
) -> Result<()> {
    if gui_state.dialog.is_some() {
        return Ok(());
    }

    if window.is_key_pressed(Key::Escape, KeyRepeat::No) {
        gui_state.active_menu = None;
    }

    let command_down = is_command_down(window);
    let pending_text_input = input_queue_has_text(input_queue);
    let ime_composition_active =
        !gui_state.editor_ime_composition.is_empty() || ime_composition_text(window).is_some();
    if should_handle_command_shortcuts(command_down, pending_text_input, ime_composition_active) {
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
            start_background_print(app, gui_state)?;
        }
        if window.is_key_pressed(Key::F, KeyRepeat::No) {
            open_find_window(app, gui_state)?;
        }
        if window.is_key_pressed(Key::H, KeyRepeat::No) {
            open_dialog_window(
                gui_state,
                DialogState::Replace {
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
                },
            )?;
        }
        if window.is_key_pressed(Key::G, KeyRepeat::No) && !app.settings().word_wrap {
            open_go_to_dialog(app, gui_state)?;
        }
        if window.is_key_pressed(Key::E, KeyRepeat::No) {
            search_with_bing(app, gui_state);
        }
        if window.is_key_pressed(Key::A, KeyRepeat::No) {
            app.select_all();
            gui_state.selection_anchor = Some(0);
            gui_state.selection_focus = Some(app.caret_offset());
        }
        if window.is_key_pressed(Key::X, KeyRepeat::No) {
            if let Some(text) = app.cut_selection()? {
                set_clipboard_text(gui_state, text);
                gui_state.selection_anchor = None;
                gui_state.selection_focus = None;
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
            gui_state.selection_anchor = None;
            gui_state.selection_focus = None;
        }
        if window.is_key_pressed(Key::Z, KeyRepeat::No) {
            app.undo()?;
            gui_state.selection_anchor = None;
            gui_state.selection_focus = None;
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
            Key::Left => move_caret_with_selection(window, app, gui_state, |app| app.move_left())?,
            Key::Right => {
                move_caret_with_selection(window, app, gui_state, |app| app.move_right())?
            }
            Key::Up => move_caret_with_selection(window, app, gui_state, |app| app.move_up())?,
            Key::Down => move_caret_with_selection(window, app, gui_state, |app| app.move_down())?,
            Key::PageUp => scroll_page(window, app, gui_state, -1)?,
            Key::PageDown => scroll_page(window, app, gui_state, 1)?,
            Key::Home => {
                move_caret_with_selection(window, app, gui_state, |app| app.move_line_start())?
            }
            Key::End => {
                move_caret_with_selection(window, app, gui_state, |app| app.move_line_end())?
            }
            Key::Backspace => {
                app.backspace()?;
                gui_state.selection_anchor = None;
                gui_state.selection_focus = None;
            }
            Key::Delete => {
                app.delete_forward()?;
                gui_state.selection_anchor = None;
                gui_state.selection_focus = None;
            }
            Key::Enter | Key::NumPadEnter => {
                app.insert_text("\n")?;
                gui_state.selection_anchor = None;
                gui_state.selection_focus = None;
            }
            Key::Tab => {
                app.insert_text("\t")?;
                gui_state.selection_anchor = None;
                gui_state.selection_focus = None;
            }
            Key::F5 => {
                app.insert_time_date()?;
                gui_state.selection_anchor = None;
                gui_state.selection_focus = None;
            }
            _ => {}
        }
    }

    if window.is_key_pressed(Key::F3, KeyRepeat::No) {
        find_again(app, gui_state, !is_shift_down(window))?;
    }
    Ok(())
}

fn should_handle_command_shortcuts(
    command_down: bool,
    pending_text_input: bool,
    ime_composition_active: bool,
) -> bool {
    command_down && !pending_text_input && !ime_composition_active
}

fn input_queue_has_text(input_queue: &Rc<RefCell<Vec<char>>>) -> bool {
    input_queue
        .borrow()
        .iter()
        .any(|character| !character.is_control())
}

fn move_caret_with_selection(
    window: &Window,
    app: &mut BlitzApp,
    gui_state: &mut GuiState,
    move_caret: impl FnOnce(&mut BlitzApp) -> Result<()>,
) -> Result<()> {
    let anchor =
        is_shift_down(window).then(|| gui_state.selection_anchor.unwrap_or(app.caret_offset()));
    move_caret(app)?;
    if let Some(anchor) = anchor {
        app.set_selection_range(anchor, app.caret_offset())?;
        gui_state.selection_anchor = Some(anchor);
        gui_state.selection_focus = Some(app.caret_offset());
    } else {
        gui_state.selection_anchor = None;
        gui_state.selection_focus = None;
    }
    Ok(())
}

fn handle_text_input(
    input_queue: &Rc<RefCell<Vec<char>>>,
    window: &Window,
    app: &mut BlitzApp,
    gui_state: &mut GuiState,
) -> Result<()> {
    if gui_state.dialog.is_some() || gui_state.find_window.is_some() {
        input_queue.borrow_mut().clear();
        return Ok(());
    }

    let characters = input_queue.borrow_mut().drain(..).collect::<Vec<_>>();
    let has_text = characters.iter().any(|character| !character.is_control());
    if is_command_down(window) && !has_text {
        return Ok(());
    }

    for character in characters {
        if !character.is_control() {
            app.insert_text(&character.to_string())?;
            gui_state.selection_anchor = None;
            gui_state.selection_focus = None;
        }
    }
    Ok(())
}

fn append_dialog_character(dialog: &mut DialogState, character: char) {
    match dialog {
        DialogState::Replace {
            query,
            replacement,
            active_field,
            ..
        } => match active_field {
            ReplaceField::Find => query.push(character),
            ReplaceField::Replace => replacement.push(character),
        },
        DialogState::GoTo {
            line,
            replace_on_input,
        } if character.is_ascii_digit() => {
            if *replace_on_input {
                line.clear();
                *replace_on_input = false;
            }
            line.push(character);
        }
        _ => {}
    }
}

fn backspace_dialog_field(dialog: &mut DialogState) {
    match dialog {
        DialogState::Replace {
            query,
            replacement,
            active_field,
            ..
        } => match active_field {
            ReplaceField::Find => {
                query.pop();
            }
            ReplaceField::Replace => {
                replacement.pop();
            }
        },
        DialogState::GoTo {
            line,
            replace_on_input,
        } => {
            if *replace_on_input {
                line.clear();
                *replace_on_input = false;
            } else {
                line.pop();
            }
        }
        _ => {}
    }
}

fn tab_dialog_field(dialog: &mut DialogState) {
    if let DialogState::Replace { active_field, .. } = dialog {
        *active_field = match active_field {
            ReplaceField::Find => ReplaceField::Replace,
            ReplaceField::Replace => ReplaceField::Find,
        };
    }
}

fn toggle_dialog_option(dialog: &mut DialogState) {
    match dialog {
        DialogState::Replace { match_case, .. } => *match_case = !*match_case,
        _ => {}
    }
}

fn paste_into_dialog_field(dialog: &mut DialogState, clipboard_cache: &mut String) {
    let pasted = clipboard_text(clipboard_cache);
    if pasted.is_empty() {
        return;
    }
    match dialog {
        DialogState::Replace {
            query,
            replacement,
            active_field,
            ..
        } => match active_field {
            ReplaceField::Find => query.push_str(&pasted),
            ReplaceField::Replace => replacement.push_str(&pasted),
        },
        DialogState::GoTo {
            line,
            replace_on_input,
        } => {
            let digits = pasted
                .chars()
                .filter(|character| character.is_ascii_digit())
                .collect::<String>();
            if !digits.is_empty() {
                if *replace_on_input {
                    line.clear();
                    *replace_on_input = false;
                }
                line.push_str(&digits);
            }
        }
        DialogState::Info { .. } => {}
    }
    *clipboard_cache = pasted;
}

fn accept_dialog_state(
    app: &mut BlitzApp,
    last_search: &mut Option<SearchSpec>,
    pending_reveal_selection: &mut bool,
    dialog: &mut DialogState,
    command_down: bool,
) -> Result<bool> {
    match dialog {
        DialogState::Replace {
            query,
            replacement,
            match_case,
            ..
        } => {
            *last_search = Some(SearchSpec {
                query: query.clone(),
                match_case: *match_case,
                wrap_around: true,
            });
            if command_down {
                let count = app.replace_all(query, replacement, *match_case)?;
                let _ = count;
            } else if app.replace_next(query, replacement, *match_case)? {
                *pending_reveal_selection = true;
            }
            Ok(true)
        }
        DialogState::GoTo { line, .. } => {
            let parsed = line.parse::<usize>().unwrap_or(0);
            if app.go_to_line(parsed)? {
                Ok(false)
            } else {
                Ok(true)
            }
        }
        DialogState::Info { .. } => Ok(false),
    }
}

#[cfg(target_os = "macos")]
fn is_command_down(window: &Window) -> bool {
    window.is_key_down(Key::LeftSuper) || window.is_key_down(Key::RightSuper)
}

#[cfg(target_os = "windows")]
fn is_command_down(_window: &Window) -> bool {
    unsafe { GetKeyState(VK_CONTROL as i32) < 0 }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn is_command_down(window: &Window) -> bool {
    window.is_key_down(Key::LeftCtrl) || window.is_key_down(Key::RightCtrl)
}

#[cfg(test)]
fn command_shortcuts_suppressed_for_text_input(command_down: bool) -> bool {
    !should_handle_command_shortcuts(command_down, true, false)
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
        ("File", "Print...") => {
            start_background_print(app, gui_state)?;
        }
        ("File", "Exit") => return confirm_unsaved_changes(app, gui_state),
        ("Edit", "Undo") => {
            app.undo()?;
            gui_state.selection_anchor = None;
            gui_state.selection_focus = None;
        }
        ("Edit", "Cut") => {
            if let Some(text) = app.cut_selection()? {
                set_clipboard_text(gui_state, text);
                gui_state.selection_anchor = None;
                gui_state.selection_focus = None;
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
            gui_state.selection_anchor = None;
            gui_state.selection_focus = None;
        }
        ("Edit", "Delete") => {
            app.delete_forward()?;
            gui_state.selection_anchor = None;
            gui_state.selection_focus = None;
        }
        ("Edit", "Select All") => {
            app.select_all();
            gui_state.selection_anchor = Some(0);
            gui_state.selection_focus = Some(app.caret_offset());
        }
        ("Edit", "Time/Date") => {
            app.insert_time_date()?;
            gui_state.selection_anchor = None;
            gui_state.selection_focus = None;
        }
        ("Edit", "Search with Bing...") => search_with_bing(app, gui_state),
        ("Edit", "Find...") => {
            open_find_window(app, gui_state)?;
        }
        ("Edit", "Find Next") => find_again(app, gui_state, true)?,
        ("Edit", "Find Previous") => find_again(app, gui_state, false)?,
        ("Edit", "Replace...") => {
            open_dialog_window(
                gui_state,
                DialogState::Replace {
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
                },
            )?;
        }
        ("Edit", "Go To...") => {
            open_go_to_dialog(app, gui_state)?;
        }
        ("Format", "Word Wrap") => {
            app.toggle_word_wrap();
            gui_state.reset_word_wrap_view();
        }
        ("Format", "Font...") => {
            open_info_window(
                gui_state,
                "Font",
                &format!("Current font stack: {}\nSample: AaBbYyZz\nAptos / Yu Gothic UI are preferred when available.", gui_state.fonts.description()),
            )?;
        }
        ("View", "Zoom In") => app.zoom_in(),
        ("View", "Zoom Out") => app.zoom_out(),
        ("View", "Restore Default Zoom") => app.restore_default_zoom(),
        ("View", "Status Bar") => app.toggle_status_bar(),
        ("Help", "View Help") => open_browser("https://github.com/", gui_state),
        ("Help", "Send Feedback") => open_browser("https://github.com/", gui_state),
        ("Help", "About Notepad") => {
            open_info_window(
                gui_state,
                "About Notepad",
                "blitzpad 0.1.0\nNotepad-compatible editor",
            )?;
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
    if app.find_text_with_options(
        &search.query,
        search.match_case,
        forward,
        search.wrap_around,
    )? {
        gui_state.pending_reveal_selection = true;
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
                    let _ = open_info_window(gui_state, "Save", &format!("Save failed: {error}"));
                }
            }
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => {
            gui_state.save_job = None;
            let _ = open_info_window(gui_state, "Save", "Save failed: worker stopped");
        }
    }
}

fn start_background_print(app: &BlitzApp, gui_state: &mut GuiState) -> Result<()> {
    if gui_state.print_job.is_some() {
        gui_state.set_message("Printing...");
        return Ok(());
    }

    let request = match print_request_for_app(app) {
        Ok(request) => request,
        Err(error) => {
            let _ = open_info_window(gui_state, "Print", &format!("Print failed: {error}"));
            return Ok(());
        }
    };
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("blitz-print".to_owned())
        .spawn(move || {
            let result = run_print_request(request);
            let _ = sender.send(PrintJobResult { result });
        })?;
    gui_state.print_job = Some(PrintJob { receiver });
    gui_state.set_message("Printing...");
    Ok(())
}

fn poll_print_job(gui_state: &mut GuiState) {
    let Some(received) = gui_state
        .print_job
        .as_ref()
        .map(|job| job.receiver.try_recv())
    else {
        return;
    };

    match received {
        Ok(result) => {
            gui_state.print_job = None;
            match result.result {
                Ok(PrintOutcome::Submitted) => gui_state.set_message("Printed"),
                Ok(PrintOutcome::Cancelled) => gui_state.set_message("Print canceled"),
                Err(error) => {
                    let _ = open_info_window(gui_state, "Print", &format!("Print failed: {error}"));
                }
            }
        }
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => {
            gui_state.print_job = None;
            let _ = open_info_window(gui_state, "Print", "Print failed: worker stopped");
        }
    }
}

fn print_request_for_app(app: &BlitzApp) -> Result<PrintRequest> {
    Ok(PrintRequest {
        document: app.document().snapshot_clone(),
        title: app.document().file_name(),
    })
}

fn run_print_request(request: PrintRequest) -> Result<PrintOutcome> {
    print_document_with_platform(&request.document, &request.title)
}

fn visit_print_lines(document: &Document, mut visit: impl FnMut(&str) -> Result<()>) -> Result<()> {
    let mut line = Vec::new();
    let mut emitted_any = false;

    document.for_each_chunk(|chunk| {
        for &byte in chunk {
            if byte == b'\n' {
                emit_print_line(&mut line, &mut visit)?;
                emitted_any = true;
            } else {
                line.push(byte);
                while line.len() >= PRINT_LINE_CHUNK_BYTES {
                    let split_at = utf8_prefix_boundary(&line, PRINT_LINE_CHUNK_BYTES);
                    if split_at == 0 {
                        break;
                    }
                    emit_print_line_part(&mut line, split_at, &mut visit)?;
                    emitted_any = true;
                }
            }
        }
        Ok::<(), BlitzError>(())
    })?;

    if !line.is_empty() || !emitted_any {
        emit_print_line(&mut line, &mut visit)?;
    }
    Ok(())
}

fn emit_print_line(line: &mut Vec<u8>, visit: &mut impl FnMut(&str) -> Result<()>) -> Result<()> {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    emit_print_line_part(line, line.len(), visit)
}

fn emit_print_line_part(
    line: &mut Vec<u8>,
    end: usize,
    visit: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let end = end.min(line.len());
    let text = String::from_utf8_lossy(&line[..end]);
    visit(&text)?;
    line.drain(..end);
    Ok(())
}

fn utf8_prefix_boundary(bytes: &[u8], max_len: usize) -> usize {
    let mut end = max_len.min(bytes.len());
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    end
}

#[cfg(target_os = "windows")]
fn print_document_with_platform(document: &Document, title: &str) -> Result<PrintOutcome> {
    gdi_print::print_document(document, title)
}

#[cfg(not(target_os = "windows"))]
fn print_document_with_platform(document: &Document, title: &str) -> Result<PrintOutcome> {
    let path = temporary_print_path(title);
    document.save_snapshot_to_path(&path, document.encoding(), document.line_ending())?;
    let status = Command::new("lp").arg(&path).status();
    let _ = std::fs::remove_file(&path);
    match status {
        Ok(status) if status.success() => Ok(PrintOutcome::Submitted),
        Ok(status) => Err(BlitzError::Window(format!(
            "print command exited with {status}"
        ))),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(target_os = "windows"))]
fn temporary_print_path(file_name: &str) -> std::path::PathBuf {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let name = sanitized_print_file_name(file_name);
    env::temp_dir().join(format!(
        "blitzpad-print-{}-{timestamp}-{name}.txt",
        std::process::id()
    ))
}

#[cfg(not(target_os = "windows"))]
fn sanitized_print_file_name(file_name: &str) -> String {
    let sanitized = file_name
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => character,
            _ => '_',
        })
        .take(64)
        .collect::<String>();
    if sanitized.trim_matches('_').is_empty() {
        "Untitled".to_owned()
    } else {
        sanitized
    }
}

#[cfg(target_os = "windows")]
mod gdi_print {
    use std::mem::{size_of, zeroed};
    use std::ptr::null;

    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        CreateFontW, DeleteDC, DeleteObject, DrawTextW, GetDeviceCaps, GetTextMetricsW,
        SelectObject, SetBkMode, SetMapMode, SetTextColor, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
        DEFAULT_QUALITY, DT_EXPANDTABS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_TOP, FF_MODERN,
        FIXED_PITCH, FW_NORMAL, HDC, HFONT, HGDIOBJ, HORZRES, LOGPIXELSX, LOGPIXELSY, MM_TEXT,
        OUT_DEFAULT_PRECIS, TEXTMETRICW, TRANSPARENT, VERTRES,
    };
    use windows_sys::Win32::UI::Controls::Dialogs::{
        CommDlgExtendedError, PrintDlgW, PD_NOPAGENUMS, PD_NOSELECTION, PD_RETURNDC,
        PD_USEDEVMODECOPIESANDCOLLATE, PRINTDLGW,
    };

    use super::{visit_print_lines, Document, PrintOutcome};
    use crate::{BlitzError, Result};

    const PRINT_POINT_SIZE: i32 = 10;

    #[repr(C)]
    struct DocInfoW {
        cb_size: i32,
        doc_name: *const u16,
        output: *const u16,
        datatype: *const u16,
        fw_type: u32,
    }

    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn StartDocW(hdc: HDC, lpdi: *const DocInfoW) -> i32;
        fn EndDoc(hdc: HDC) -> i32;
        fn StartPage(hdc: HDC) -> i32;
        fn EndPage(hdc: HDC) -> i32;
        fn AbortDoc(hdc: HDC) -> i32;
    }

    pub(super) fn print_document(document: &Document, title: &str) -> Result<PrintOutcome> {
        unsafe {
            let mut dialog: PRINTDLGW = zeroed();
            dialog.lStructSize = size_of::<PRINTDLGW>() as u32;
            dialog.Flags =
                PD_RETURNDC | PD_NOSELECTION | PD_NOPAGENUMS | PD_USEDEVMODECOPIESANDCOLLATE;

            if PrintDlgW(&mut dialog) == 0 {
                let error = CommDlgExtendedError();
                return if error == 0 {
                    Ok(PrintOutcome::Cancelled)
                } else {
                    Err(BlitzError::Window(format!(
                        "PrintDlgW failed with common dialog error {error}"
                    )))
                };
            }

            let hdc = dialog.hDC;
            let result = if hdc.is_null() {
                Err(BlitzError::Window(
                    "print dialog did not return a printer device context".to_owned(),
                ))
            } else {
                print_to_hdc(hdc, document, title)
            };

            if !hdc.is_null() {
                DeleteDC(hdc);
            }
            if !dialog.hDevMode.is_null() {
                GlobalFree(dialog.hDevMode);
            }
            if !dialog.hDevNames.is_null() {
                GlobalFree(dialog.hDevNames);
            }

            result
        }
    }

    unsafe fn print_to_hdc(hdc: HDC, document: &Document, title: &str) -> Result<PrintOutcome> {
        let title_wide = wide_null(title);
        let doc_info = DocInfoW {
            cb_size: size_of::<DocInfoW>() as i32,
            doc_name: title_wide.as_ptr(),
            output: null(),
            datatype: null(),
            fw_type: 0,
        };

        if StartDocW(hdc, &doc_info) <= 0 {
            return Err(BlitzError::Window("StartDocW failed".to_owned()));
        }

        let print_result = print_pages(hdc, document);
        match print_result {
            Ok(()) => {
                if EndDoc(hdc) <= 0 {
                    Err(BlitzError::Window("EndDoc failed".to_owned()))
                } else {
                    Ok(PrintOutcome::Submitted)
                }
            }
            Err(error) => {
                AbortDoc(hdc);
                Err(error)
            }
        }
    }

    unsafe fn print_pages(hdc: HDC, document: &Document) -> Result<()> {
        SetMapMode(hdc, MM_TEXT as i32);
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, 0x00000000);

        let log_pixels_x = GetDeviceCaps(hdc, LOGPIXELSX as i32).max(96);
        let log_pixels_y = GetDeviceCaps(hdc, LOGPIXELSY as i32).max(96);
        let page_width = GetDeviceCaps(hdc, HORZRES as i32).max(log_pixels_x);
        let page_height = GetDeviceCaps(hdc, VERTRES as i32).max(log_pixels_y);
        let margin_x = log_pixels_x / 2;
        let margin_y = log_pixels_y / 2;
        let left = margin_x;
        let right = (page_width - margin_x).max(left + 1);
        let top = margin_y;
        let bottom = (page_height - margin_y).max(top + 1);

        let face = wide_null("Consolas");
        let font_height = -((PRINT_POINT_SIZE * log_pixels_y) / 72).max(1);
        let font = CreateFontW(
            font_height,
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            DEFAULT_QUALITY as u32,
            (FIXED_PITCH | FF_MODERN) as u32,
            face.as_ptr(),
        );
        let old_font = if font.is_null() {
            std::ptr::null_mut()
        } else {
            SelectObject(hdc, font)
        };

        let mut metrics: TEXTMETRICW = zeroed();
        let line_height = if GetTextMetricsW(hdc, &mut metrics) != 0 {
            (metrics.tmHeight + metrics.tmExternalLeading).max(1)
        } else {
            (log_pixels_y / 6).max(1)
        };

        let result = {
            let mut y = top;
            let mut page_open = false;
            let result = (|| -> Result<()> {
                start_print_page(hdc)?;
                page_open = true;

                visit_print_lines(document, |line| {
                    unsafe {
                        if y + line_height > bottom {
                            end_print_page(hdc)?;
                            page_open = false;
                            start_print_page(hdc)?;
                            page_open = true;
                            y = top;
                        }

                        if !line.is_empty() {
                            let line_wide = wide(line);
                            let mut rect = RECT {
                                left,
                                top: y,
                                right,
                                bottom: (y + line_height).min(bottom),
                            };
                            if DrawTextW(
                                hdc,
                                line_wide.as_ptr(),
                                line_wide.len() as i32,
                                &mut rect,
                                DT_LEFT | DT_TOP | DT_SINGLELINE | DT_EXPANDTABS | DT_NOPREFIX,
                            ) == 0
                            {
                                return Err(BlitzError::Window("DrawTextW failed".to_owned()));
                            }
                        }
                        y += line_height;
                    }
                    Ok(())
                })?;

                if page_open {
                    end_print_page(hdc)?;
                    page_open = false;
                }
                Ok(())
            })();
            result
        };
        restore_font(hdc, old_font, font);
        result
    }

    unsafe fn start_print_page(hdc: HDC) -> Result<()> {
        if StartPage(hdc) <= 0 {
            Err(BlitzError::Window("StartPage failed".to_owned()))
        } else {
            Ok(())
        }
    }

    unsafe fn end_print_page(hdc: HDC) -> Result<()> {
        if EndPage(hdc) <= 0 {
            Err(BlitzError::Window("EndPage failed".to_owned()))
        } else {
            Ok(())
        }
    }

    unsafe fn restore_font(hdc: HDC, old_font: HGDIOBJ, font: HFONT) {
        if !old_font.is_null() {
            SelectObject(hdc, old_font);
        }
        if !font.is_null() {
            DeleteObject(font);
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn wide_null(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
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

fn clipboard_text(clipboard_cache: &str) -> String {
    Clipboard::new()
        .ok()
        .and_then(|mut clipboard| clipboard.get_text().ok())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| clipboard_cache.to_owned())
}

fn clipboard_has_text(clipboard_cache: &str) -> bool {
    !clipboard_cache.is_empty()
        || Clipboard::new()
            .ok()
            .and_then(|mut clipboard| clipboard.get_text().ok())
            .is_some_and(|text| !text.is_empty())
}

fn paste_into_find_query(query: &mut String, clipboard_cache: &mut String) {
    let pasted = clipboard_text(clipboard_cache);
    append_find_query_paste(query, clipboard_cache, pasted);
}

fn append_find_query_paste(query: &mut String, clipboard_cache: &mut String, pasted: String) {
    if !pasted.is_empty() {
        query.push_str(&pasted);
        *clipboard_cache = pasted;
    }
}

fn paste_from_clipboard(app: &mut BlitzApp, gui_state: &mut GuiState) -> Result<()> {
    let clipboard_text = clipboard_text(&gui_state.clipboard);

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
        for cell in status_bar_cells(state, gui_state) {
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

fn status_bar_cells(state: &NotepadUiState, gui_state: &GuiState) -> Vec<String> {
    let mut cells = state.status_cells();
    if let Some(message) = gui_state
        .status_text()
        .filter(|message| !message.is_empty())
    {
        cells.insert(0, message.to_owned());
    }
    cells
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
    if let DialogState::GoTo {
        line,
        replace_on_input,
    } = dialog
    {
        draw_go_to_dialog(canvas, fonts, line, *replace_on_input);
        return;
    }

    let width = 430usize.min(canvas.width.saturating_sub(40));
    let height = match dialog {
        DialogState::Replace { .. } => 180,
        DialogState::Info { .. } => 160,
        DialogState::GoTo { .. } => unreachable!(),
    };
    let x = (canvas.width.saturating_sub(width)) / 2;
    let y = (canvas.height.saturating_sub(height)) / 2;

    canvas.fill_rect(x + 4, y + 4, width, height, 0x00909090);
    canvas.fill_rect(x, y, width, height, COLOR_DROPDOWN);
    canvas.rect(x, y, width, height, COLOR_BORDER);

    match dialog {
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
        DialogState::GoTo { .. } => unreachable!(),
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

fn go_to_dialog_line(app: &BlitzApp) -> Result<String> {
    Ok(app.ui_state()?.caret_line.to_string())
}

fn open_go_to_dialog(app: &BlitzApp, gui_state: &mut GuiState) -> Result<()> {
    open_dialog_window(
        gui_state,
        DialogState::GoTo {
            line: go_to_dialog_line(app)?,
            replace_on_input: true,
        },
    )
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
    editor_metrics_from_flags(state.status_bar_visible, state.word_wrap, width, height)
}

fn editor_metrics_for_app(app: &BlitzApp, width: usize, height: usize) -> Option<EditorMetrics> {
    editor_metrics_from_flags(
        app.settings().status_bar_visible,
        app.settings().word_wrap,
        width,
        height,
    )
}

fn editor_metrics_from_flags(
    status_bar_visible: bool,
    word_wrap: bool,
    width: usize,
    height: usize,
) -> Option<EditorMetrics> {
    let status_height = if status_bar_visible { STATUS_HEIGHT } else { 0 };
    let text_top = text_top();
    let text_bottom = height.saturating_sub(status_height + 1);
    if text_bottom <= text_top {
        return None;
    }

    let text_width = width.saturating_sub(SCROLLBAR_WIDTH);
    let horizontal_scroll_y = (!word_wrap && text_bottom > text_top + SCROLLBAR_WIDTH)
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

fn max_first_visible_line_for_metrics(
    app: &BlitzApp,
    gui_state: &GuiState,
    metrics: EditorMetrics,
) -> usize {
    vertical_content_len(app, gui_state, metrics).saturating_sub(metrics.visible_line_count)
}

fn vertical_content_len(app: &BlitzApp, gui_state: &GuiState, metrics: EditorMetrics) -> usize {
    if app.settings().word_wrap {
        with_wrapped_row_index(app, gui_state, metrics, |index| index.total_rows)
    } else {
        app.document().line_count()
    }
    .max(1)
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

fn centered_offset_for_range(range_start: usize, range_end: usize, visible_len: usize) -> usize {
    let visible_len = visible_len.max(1);
    let range_len = range_end.saturating_sub(range_start);
    if range_len >= visible_len {
        return range_start;
    }

    let midpoint = range_start + range_len / 2;
    let centered = midpoint.saturating_sub(visible_len / 2);
    let latest_offset_that_keeps_end_visible = range_end.saturating_sub(visible_len);
    centered
        .max(latest_offset_that_keeps_end_visible)
        .min(range_start)
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
    let content_len = vertical_content_len(app, gui_state, metrics);
    let (thumb_y, thumb_height) = scroll_thumb(
        metrics.text_top,
        metrics.text_height,
        content_len,
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
        .min(max_first_visible_line_for_metrics(app, gui_state, metrics));
    let visible_lines =
        editor_visible_lines(app, first_visible_line, metrics, gui_state, state.word_wrap);
    draw_selection_highlights(canvas, app, &visible_lines, metrics, fonts);
    for (index, line) in visible_lines.iter().enumerate() {
        let y = metrics.text_top + TEXT_MARGIN_Y + index * EDITOR_LINE_HEIGHT;
        canvas.clipped_text(
            TEXT_MARGIN_X,
            y,
            &line.text,
            COLOR_TEXT,
            TextRole::Editor,
            fonts,
            metrics.editor_text_width,
        );
    }

    if visible_lines.is_empty() {
        draw_caret(canvas, TEXT_MARGIN_X, metrics.text_top + TEXT_MARGIN_Y);
    } else if let Some(visible_index) =
        visible_line_index_for_caret(&visible_lines, state.caret_line, app.caret_offset())
    {
        let line = &visible_lines[visible_index];
        let local_offset = app
            .caret_offset()
            .saturating_sub(line.byte_range.start)
            .min(line.text.len());
        let prefix = line.text.get(..local_offset).unwrap_or(&line.text);
        let visible_prefix =
            text_prefix_for_width(fonts, prefix, TextRole::Editor, metrics.editor_text_width);
        let caret_x = (TEXT_MARGIN_X + fonts.measure(visible_prefix, TextRole::Editor))
            .min(TEXT_MARGIN_X + metrics.editor_text_width);
        let y = metrics.text_top + TEXT_MARGIN_Y + visible_index * EDITOR_LINE_HEIGHT;
        draw_caret(canvas, caret_x, y);
    }
}

fn visible_line_index_for_caret(
    visible_lines: &[VisibleLine],
    caret_line: usize,
    caret_offset: usize,
) -> Option<usize> {
    visible_lines
        .iter()
        .rposition(|line| {
            line.number == caret_line
                && caret_offset >= line.byte_range.start
                && caret_offset < line.byte_range.end
        })
        .or_else(|| {
            visible_lines
                .iter()
                .rposition(|line| line.number == caret_line && caret_offset == line.byte_range.end)
        })
}

fn editor_visible_lines(
    app: &BlitzApp,
    first_visible_line: usize,
    metrics: EditorMetrics,
    gui_state: &GuiState,
    word_wrap: bool,
) -> Vec<VisibleLine> {
    if !word_wrap {
        return app.document().visible_lines_at(
            first_visible_line,
            metrics.visible_line_count,
            gui_state.horizontal_offset,
        );
    }

    wrapped_visible_lines(
        app,
        first_visible_line,
        metrics.visible_line_count,
        gui_state,
        metrics,
    )
}

fn wrapped_visible_lines(
    app: &BlitzApp,
    first_visible_row: usize,
    max_rows: usize,
    gui_state: &GuiState,
    metrics: EditorMetrics,
) -> Vec<VisibleLine> {
    let mut rows = Vec::with_capacity(max_rows);
    let (mut document_line, mut rows_to_skip) =
        with_wrapped_row_index(app, gui_state, metrics, |index| {
            wrapped_line_for_visual_row(index, first_visible_row)
        });
    let line_count = app.document().line_count();

    while rows.len() < max_rows && document_line < line_count {
        append_wrapped_document_line_segments(
            app,
            document_line,
            &mut rows_to_skip,
            &mut rows,
            max_rows,
            &gui_state.fonts,
            metrics.editor_text_width,
        );
        document_line += 1;
    }

    rows
}

fn append_wrapped_document_line_segments(
    app: &BlitzApp,
    document_line: usize,
    rows_to_skip: &mut usize,
    rows: &mut Vec<VisibleLine>,
    max_rows: usize,
    fonts: &FontStack,
    max_width: usize,
) {
    let Some(content_range) = document_line_content_range(app, document_line) else {
        return;
    };
    if content_range.is_empty() {
        append_wrapped_segment(
            rows_to_skip,
            rows,
            max_rows,
            VisibleLine {
                number: document_line + 1,
                byte_range: content_range.clone(),
                text: String::new(),
            },
        );
        return;
    }

    let mut local_offset = 0usize;
    while content_range.start + local_offset < content_range.end && rows.len() < max_rows {
        let Some(chunk) = app
            .document()
            .visible_lines_at(document_line, 1, local_offset)
            .into_iter()
            .next()
        else {
            break;
        };
        if chunk.byte_range.is_empty() {
            break;
        }
        let next_local_offset = chunk.byte_range.end.saturating_sub(content_range.start);
        append_wrapped_line_segments(rows_to_skip, rows, chunk, max_rows, fonts, max_width);
        if next_local_offset <= local_offset {
            break;
        }
        local_offset = next_local_offset;
    }
}

fn append_wrapped_line_segments(
    rows_to_skip: &mut usize,
    rows: &mut Vec<VisibleLine>,
    line: VisibleLine,
    max_rows: usize,
    fonts: &FontStack,
    max_width: usize,
) {
    if line.text.is_empty() {
        append_wrapped_segment(rows_to_skip, rows, max_rows, line);
        return;
    }

    let mut segment_start = 0usize;
    while segment_start < line.text.len() && rows.len() < max_rows {
        let segment_end = wrapped_segment_end(fonts, &line.text, segment_start, max_width);
        debug_assert!(segment_end > segment_start);
        let text = line.text[segment_start..segment_end].to_owned();
        append_wrapped_segment(
            rows_to_skip,
            rows,
            max_rows,
            VisibleLine {
                number: line.number,
                byte_range: line.byte_range.start + segment_start
                    ..line.byte_range.start + segment_end,
                text,
            },
        );
        segment_start = segment_end;
    }
}

fn append_wrapped_segment(
    rows_to_skip: &mut usize,
    rows: &mut Vec<VisibleLine>,
    max_rows: usize,
    segment: VisibleLine,
) {
    if *rows_to_skip > 0 {
        *rows_to_skip -= 1;
    } else if rows.len() < max_rows {
        rows.push(segment);
    }
}

fn wrapped_segment_end(
    fonts: &FontStack,
    text: &str,
    segment_start: usize,
    max_width: usize,
) -> usize {
    let limit = max_width.max(1) as f32;
    let mut width = 0.0f32;
    let mut last_end = segment_start;

    for unit in text_units(&text[segment_start..]) {
        let absolute_start = segment_start + unit.start;
        let absolute_end = segment_start + unit.end;
        let advance = fonts.measure_unit(unit, TextRole::Editor);
        if width + advance > limit {
            return if absolute_start == segment_start {
                absolute_end
            } else {
                last_end
            };
        }
        width += advance;
        last_end = absolute_end;
    }

    text.len()
}

fn with_wrapped_row_index<T>(
    app: &BlitzApp,
    gui_state: &GuiState,
    metrics: EditorMetrics,
    visit: impl FnOnce(&WrappedRowIndex) -> T,
) -> T {
    let generation = app.document().change_generation();
    let line_count = app.document().line_count();
    let editor_text_width = metrics.editor_text_width;
    // Wrapped row starts depend only on document content, line index shape, and wrap width.
    let needs_rebuild = gui_state
        .wrapped_row_index
        .borrow()
        .as_ref()
        .is_none_or(|index| {
            index.generation != generation
                || index.line_count != line_count
                || index.editor_text_width != editor_text_width
        });

    if needs_rebuild {
        let mut row_starts = Vec::with_capacity(line_count);
        let mut total_rows = 0usize;
        for line in 0..line_count {
            row_starts.push(total_rows);
            total_rows = total_rows.saturating_add(wrapped_line_row_count(
                app,
                line,
                &gui_state.fonts,
                editor_text_width,
            ));
        }
        *gui_state.wrapped_row_index.borrow_mut() = Some(WrappedRowIndex {
            generation,
            line_count,
            editor_text_width,
            row_starts,
            total_rows: total_rows.max(1),
        });
    }

    let index = gui_state.wrapped_row_index.borrow();
    visit(index.as_ref().expect("wrapped row index initialized"))
}

fn wrapped_line_for_visual_row(index: &WrappedRowIndex, visual_row: usize) -> (usize, usize) {
    if index.row_starts.is_empty() {
        return (0, 0);
    }

    let clamped_row = visual_row.min(index.total_rows.saturating_sub(1));
    let line = index
        .row_starts
        .partition_point(|start| *start <= clamped_row)
        .saturating_sub(1)
        .min(index.row_starts.len().saturating_sub(1));
    (line, clamped_row.saturating_sub(index.row_starts[line]))
}

fn wrapped_line_row_count(
    app: &BlitzApp,
    document_line: usize,
    fonts: &FontStack,
    max_width: usize,
) -> usize {
    let Some(content_range) = document_line_content_range(app, document_line) else {
        return 0;
    };
    if content_range.is_empty() {
        return 1;
    }

    let mut rows = 0usize;
    let mut local_offset = 0usize;
    while content_range.start + local_offset < content_range.end {
        let Some(chunk) = app
            .document()
            .visible_lines_at(document_line, 1, local_offset)
            .into_iter()
            .next()
        else {
            break;
        };
        if chunk.byte_range.is_empty() {
            break;
        }
        rows += wrapped_text_row_count(fonts, &chunk.text, max_width);
        let next_local_offset = chunk.byte_range.end.saturating_sub(content_range.start);
        if next_local_offset <= local_offset {
            break;
        }
        local_offset = next_local_offset;
    }

    rows.max(1)
}

fn wrapped_text_row_count(fonts: &FontStack, text: &str, max_width: usize) -> usize {
    if text.is_empty() {
        return 1;
    }

    let mut rows = 0usize;
    let mut segment_start = 0usize;
    while segment_start < text.len() {
        let segment_end = wrapped_segment_end(fonts, text, segment_start, max_width);
        debug_assert!(segment_end > segment_start);
        if segment_end <= segment_start {
            break;
        }
        rows += 1;
        segment_start = segment_end;
    }
    rows.max(1)
}

fn wrapped_visual_row_for_offset(
    app: &BlitzApp,
    gui_state: &GuiState,
    metrics: EditorMetrics,
    byte_offset: usize,
) -> Result<usize> {
    let document_line = app.document().line_for_offset(byte_offset)?;
    let preceding_rows = with_wrapped_row_index(app, gui_state, metrics, |index| {
        index.row_starts.get(document_line).copied().unwrap_or(0)
    });

    Ok(preceding_rows
        + wrapped_row_in_line_for_offset(
            app,
            document_line,
            &gui_state.fonts,
            metrics.editor_text_width,
            byte_offset,
        ))
}

fn wrapped_row_in_line_for_offset(
    app: &BlitzApp,
    document_line: usize,
    fonts: &FontStack,
    max_width: usize,
    byte_offset: usize,
) -> usize {
    let Some(content_range) = document_line_content_range(app, document_line) else {
        return 0;
    };
    if content_range.is_empty() || byte_offset <= content_range.start {
        return 0;
    }

    let target_offset = byte_offset.min(content_range.end);
    let mut row = 0usize;
    let mut local_offset = 0usize;
    while content_range.start + local_offset < content_range.end {
        let Some(chunk) = app
            .document()
            .visible_lines_at(document_line, 1, local_offset)
            .into_iter()
            .next()
        else {
            break;
        };
        if chunk.byte_range.is_empty() {
            break;
        }

        let mut segment_start = 0usize;
        while segment_start < chunk.text.len() {
            let segment_end = wrapped_segment_end(fonts, &chunk.text, segment_start, max_width);
            debug_assert!(segment_end > segment_start);
            if segment_end <= segment_start {
                return row;
            }
            let absolute_end = chunk.byte_range.start + segment_end;
            if target_offset < absolute_end {
                return row;
            }
            row += 1;
            segment_start = segment_end;
        }

        let next_local_offset = chunk.byte_range.end.saturating_sub(content_range.start);
        if next_local_offset <= local_offset {
            break;
        }
        local_offset = next_local_offset;
    }

    row.saturating_sub(1)
}

fn document_line_content_range(
    app: &BlitzApp,
    document_line: usize,
) -> Option<std::ops::Range<usize>> {
    let line_start = app.document().line_start(document_line)?;
    app.document()
        .line_content_range_for_offset(line_start)
        .ok()
}

fn draw_selection_highlights(
    canvas: &mut Canvas,
    app: &BlitzApp,
    visible_lines: &[VisibleLine],
    metrics: EditorMetrics,
    fonts: &FontStack,
) {
    let Some(selection) = app.selected_range() else {
        return;
    };

    for (index, line) in visible_lines.iter().enumerate() {
        let start = selection.start.max(line.byte_range.start);
        let end = selection.end.min(line.byte_range.end);
        if start >= end {
            continue;
        }

        let local_start = start
            .saturating_sub(line.byte_range.start)
            .min(line.text.len());
        let local_end = end
            .saturating_sub(line.byte_range.start)
            .min(line.text.len());
        let Some(prefix) = line.text.get(..local_start) else {
            continue;
        };
        let Some(selected) = line.text.get(local_start..local_end) else {
            continue;
        };
        let x = TEXT_MARGIN_X + fonts.measure(prefix, TextRole::Editor);
        let selection_right = (x + fonts.measure(selected, TextRole::Editor).max(1))
            .min(TEXT_MARGIN_X + metrics.editor_text_width);
        if selection_right <= x {
            continue;
        }
        let width = selection_right - x;
        let y = metrics.text_top + TEXT_MARGIN_Y + index * EDITOR_LINE_HEIGHT;
        canvas.fill_rect(x, y, width, EDITOR_LINE_HEIGHT, COLOR_SELECTION);
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
    for unit in text_units(text) {
        let next_width = width + fonts.measure_unit(unit, role);
        if next_width > limit {
            return &text[..unit.start];
        }
        width = next_width;
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
    let metrics = editor_metrics_for_app(app, width, height)?;
    if y < metrics.text_top + TEXT_MARGIN_Y || y >= metrics.editor_bottom || x >= metrics.text_width
    {
        return None;
    }

    let line_index = (y - metrics.text_top - TEXT_MARGIN_Y) / EDITOR_LINE_HEIGHT;
    let first_visible_line = gui_state
        .first_visible_line
        .min(max_first_visible_line_for_metrics(app, gui_state, metrics));
    let visible_lines = editor_visible_lines(
        app,
        first_visible_line,
        metrics,
        gui_state,
        app.settings().word_wrap,
    );
    let Some(line) = visible_lines.get(line_index) else {
        return Some(0);
    };
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
    for unit in text_units(line) {
        // TextUnit boundaries come from char_indices and emoji cluster parsing, so offsets stay UTF-8 safe.
        let advance = fonts.measure_unit(unit, TextRole::Editor);
        if target_x < cursor + advance / 2.0 {
            return line_start + unit.start;
        }
        cursor += advance;
        if target_x < cursor {
            return line_start + unit.end;
        }
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
    Find,
    Editor,
}

#[derive(Clone)]
struct LoadedFont {
    name: String,
    font: FontArc,
    data: Arc<[u8]>,
    face_index: usize,
}

struct FontStack {
    fonts: Vec<LoadedFont>,
    glyph_cache: RefCell<HashMap<GlyphCacheKey, GlyphMetrics>>,
    emoji_cluster_cache: RefCell<HashMap<TextRole, HashMap<String, Option<EmojiClusterMetrics>>>>,
    color_glyph_cache: RefCell<HashMap<ColorGlyphCacheKey, Option<CachedColorGlyphImage>>>,
    scale_context: RefCell<ScaleContext>,
    shape_context: RefCell<ShapeContext>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct GlyphCacheKey {
    character: char,
    role: TextRole,
}

#[derive(Clone, Copy, Debug)]
struct GlyphMetrics {
    font_index: usize,
    glyph_id: AbGlyphId,
    advance: f32,
}

#[derive(Clone, Debug)]
struct EmojiClusterMetrics {
    font_index: usize,
    glyphs: Vec<EmojiClusterGlyph>,
    advance: f32,
}

#[derive(Clone, Copy, Debug)]
struct EmojiClusterGlyph {
    id: SwashGlyphId,
    x: f32,
    y: f32,
    advance: f32,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ColorGlyphCacheKey {
    font_index: usize,
    glyph_id: SwashGlyphId,
    size_bits: u32,
}

#[derive(Clone, Debug)]
struct CachedColorGlyphImage {
    left: i32,
    top: i32,
    width: usize,
    height: usize,
    data: Arc<[u8]>,
}

impl CachedColorGlyphImage {
    fn from_swash(image: swash::scale::image::Image) -> Self {
        Self {
            left: image.placement.left,
            top: image.placement.top,
            width: image.placement.width as usize,
            height: image.placement.height as usize,
            data: Arc::from(image.data.into_boxed_slice()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TextUnit<'a> {
    text: &'a str,
    first: char,
    start: usize,
    end: usize,
    kind: TextUnitKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TextUnitKind {
    Character,
    EmojiCluster,
    Ignorable,
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
            emoji_cluster_cache: RefCell::new(HashMap::new()),
            color_glyph_cache: RefCell::new(HashMap::new()),
            scale_context: RefCell::new(ScaleContext::new()),
            shape_context: RefCell::new(ShapeContext::new()),
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
            TextRole::Find => FIND_FONT_SIZE,
            TextRole::Editor => EDITOR_FONT_SIZE,
        }
    }

    fn measure(&self, text: &str, role: TextRole) -> usize {
        self.measure_text(text, role).round() as usize
    }

    fn measure_text(&self, text: &str, role: TextRole) -> f32 {
        text_units(text)
            .map(|unit| self.measure_unit(unit, role))
            .sum()
    }

    fn measure_unit(&self, unit: TextUnit<'_>, role: TextRole) -> f32 {
        match unit.kind {
            TextUnitKind::EmojiCluster => self
                .emoji_cluster_metrics(unit.text, role)
                .map(|metrics| metrics.advance)
                .unwrap_or_else(|| self.measure_text_by_char(unit.text, role)),
            TextUnitKind::Ignorable => 0.0,
            TextUnitKind::Character => self.measure_char(unit.first, role),
        }
    }

    fn measure_text_by_char(&self, text: &str, role: TextRole) -> f32 {
        text.chars()
            .filter(|character| !is_default_ignorable_for_display(*character))
            .map(|character| self.measure_char(character, role))
            .sum()
    }

    fn measure_char(&self, character: char, role: TextRole) -> f32 {
        if character == '\t' {
            return self.measure_char(' ', role) * 4.0;
        }
        if is_default_ignorable_for_display(character) {
            return 0.0;
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
        for unit in text_units(text) {
            match unit.kind {
                TextUnitKind::EmojiCluster => {
                    if let Some(metrics) = self.emoji_cluster_metrics(unit.text, role) {
                        self.draw_emoji_cluster(canvas, &metrics, cursor, baseline, role);
                        cursor += metrics.advance;
                    } else {
                        cursor += self
                            .draw_text_by_char(canvas, cursor, baseline, unit.text, color, role);
                    }
                }
                TextUnitKind::Ignorable => {}
                TextUnitKind::Character => {
                    if unit.first == '\t' {
                        cursor += self.measure_char(unit.first, role);
                    } else {
                        let metrics = self.glyph_metrics(unit.first, role);
                        self.draw_outline_or_missing(
                            canvas, unit.first, &metrics, cursor, baseline, size, color,
                        );
                        cursor += metrics.advance;
                    }
                }
            }
        }
    }

    fn draw_text_clipped(
        &self,
        canvas: &mut Canvas,
        x: f32,
        baseline: f32,
        text: &str,
        color: u32,
        role: TextRole,
        max_width: usize,
    ) {
        let max_x = x + max_width as f32;
        let size = self.size(role);
        let mut cursor = x;
        for unit in text_units(text) {
            match unit.kind {
                TextUnitKind::EmojiCluster => {
                    if let Some(metrics) = self.emoji_cluster_metrics(unit.text, role) {
                        if cursor + metrics.advance > max_x {
                            break;
                        }
                        self.draw_emoji_cluster(canvas, &metrics, cursor, baseline, role);
                        cursor += metrics.advance;
                    } else {
                        let advance = self.measure_text_by_char(unit.text, role);
                        if cursor + advance > max_x {
                            break;
                        }
                        cursor += self
                            .draw_text_by_char(canvas, cursor, baseline, unit.text, color, role);
                    }
                }
                TextUnitKind::Ignorable => {}
                TextUnitKind::Character => {
                    if unit.first == '\t' {
                        let advance = self.measure_char(unit.first, role);
                        if cursor + advance > max_x {
                            break;
                        }
                        cursor += advance;
                    } else {
                        let metrics = self.glyph_metrics(unit.first, role);
                        if cursor + metrics.advance > max_x {
                            break;
                        }
                        self.draw_outline_or_missing(
                            canvas, unit.first, &metrics, cursor, baseline, size, color,
                        );
                        cursor += metrics.advance;
                    }
                }
            }
        }
    }

    fn draw_text_by_char(
        &self,
        canvas: &mut Canvas,
        mut cursor: f32,
        baseline: f32,
        text: &str,
        color: u32,
        role: TextRole,
    ) -> f32 {
        let start = cursor;
        let size = self.size(role);
        for character in text.chars() {
            if is_default_ignorable_for_display(character) {
                continue;
            }
            if character == '\t' {
                cursor += self.measure_char(character, role);
                continue;
            }
            let metrics = self.glyph_metrics(character, role);
            self.draw_outline_or_missing(
                canvas, character, &metrics, cursor, baseline, size, color,
            );
            cursor += metrics.advance;
        }
        cursor - start
    }

    fn draw_outline_or_missing(
        &self,
        canvas: &mut Canvas,
        character: char,
        metrics: &GlyphMetrics,
        cursor: f32,
        baseline: f32,
        size: f32,
        color: u32,
    ) {
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
        } else if !character.is_whitespace() {
            draw_missing_glyph_box(canvas, cursor, baseline, metrics.advance, size, color);
        }
    }

    fn glyph_metrics(&self, character: char, role: TextRole) -> GlyphMetrics {
        let key = GlyphCacheKey { character, role };
        if let Some(metrics) = self.glyph_cache.borrow().get(&key) {
            return *metrics;
        }

        let (font_index, glyph_id, _color_glyph_id) = self.font_index_for(character, role);
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

    fn font_index_for(&self, character: char, role: TextRole) -> (usize, AbGlyphId, SwashGlyphId) {
        let size = self.size(role);
        let first_supported = self.fonts.iter().enumerate().find_map(|(index, loaded)| {
            let glyph_id = loaded.font.glyph_id(character);
            let color_glyph_id = self.swash_glyph_id(index, character);
            (glyph_id != AbGlyphId(0) || color_glyph_id != 0).then_some((
                index,
                glyph_id,
                color_glyph_id,
            ))
        });

        if character.is_whitespace() {
            return first_supported.unwrap_or_else(|| {
                (
                    0,
                    self.fonts[0].font.glyph_id(character),
                    self.swash_glyph_id(0, character),
                )
            });
        }

        self.fonts
            .iter()
            .enumerate()
            .find_map(|(index, loaded)| {
                let glyph_id = loaded.font.glyph_id(character);
                let color_glyph_id = self.swash_glyph_id(index, character);
                if glyph_id == AbGlyphId(0) && color_glyph_id == 0 {
                    return None;
                }
                if is_color_emoji_candidate(character)
                    && color_glyph_id != 0
                    && self
                        .color_glyph_image(index, color_glyph_id, role)
                        .is_some()
                {
                    return Some((index, glyph_id, color_glyph_id));
                }
                let glyph = glyph_id.with_scale_and_position(size, point(0.0, 0.0));
                loaded.font.outline_glyph(glyph).is_some().then_some((
                    index,
                    glyph_id,
                    color_glyph_id,
                ))
            })
            .or(first_supported)
            .unwrap_or_else(|| {
                (
                    0,
                    self.fonts[0].font.glyph_id(character),
                    self.swash_glyph_id(0, character),
                )
            })
    }

    fn swash_font(&self, font_index: usize) -> Option<FontRef<'_>> {
        let loaded = self.fonts.get(font_index)?;
        FontRef::from_index(&loaded.data, loaded.face_index)
    }

    fn swash_glyph_id(&self, font_index: usize, character: char) -> SwashGlyphId {
        self.swash_font(font_index)
            .map(|font| font.charmap().map(character))
            .unwrap_or(0)
    }

    fn color_glyph_image(
        &self,
        font_index: usize,
        glyph_id: SwashGlyphId,
        role: TextRole,
    ) -> Option<CachedColorGlyphImage> {
        self.color_glyph_image_with_size(font_index, glyph_id, self.size(role))
    }

    fn color_glyph_image_with_size(
        &self,
        font_index: usize,
        glyph_id: SwashGlyphId,
        size: f32,
    ) -> Option<CachedColorGlyphImage> {
        if glyph_id == 0 {
            return None;
        }

        let key = ColorGlyphCacheKey {
            font_index,
            glyph_id,
            size_bits: size.to_bits(),
        };
        if let Some(image) = self.color_glyph_cache.borrow().get(&key) {
            return image.clone();
        }

        let image = self.render_color_glyph_image(font_index, glyph_id, size);
        self.color_glyph_cache
            .borrow_mut()
            .insert(key, image.clone());
        image
    }

    fn render_color_glyph_image(
        &self,
        font_index: usize,
        glyph_id: SwashGlyphId,
        size: f32,
    ) -> Option<CachedColorGlyphImage> {
        let font = self.swash_font(font_index)?;
        let mut context = self.scale_context.borrow_mut();
        let mut scaler = context.builder(font).size(size).hint(true).build();
        Render::new(&[
            Source::ColorOutline(0),
            Source::ColorBitmap(StrikeWith::BestFit),
        ])
        .render(&mut scaler, glyph_id)
        .filter(|image| image.content == Content::Color)
        .map(CachedColorGlyphImage::from_swash)
    }

    fn emoji_cluster_metrics(&self, text: &str, role: TextRole) -> Option<EmojiClusterMetrics> {
        if let Some(metrics) = self
            .emoji_cluster_cache
            .borrow()
            .get(&role)
            .and_then(|by_text| by_text.get(text))
        {
            return metrics.clone();
        }

        let metrics = self.shape_emoji_cluster(text, role);
        self.emoji_cluster_cache
            .borrow_mut()
            .entry(role)
            .or_default()
            .insert(text.to_owned(), metrics.clone());
        metrics
    }

    fn shape_emoji_cluster(&self, text: &str, role: TextRole) -> Option<EmojiClusterMetrics> {
        for font_index in 0..self.fonts.len() {
            let Some(font) = self.swash_font(font_index) else {
                continue;
            };
            let mut glyphs = Vec::new();
            {
                let mut context = self.shape_context.borrow_mut();
                let mut shaper = context
                    .builder(font)
                    .script(Script::Latin)
                    .direction(Direction::LeftToRight)
                    .size(self.emoji_size(role))
                    .build();
                shaper.add_str(text);
                shaper.shape_with(|cluster| {
                    glyphs.extend(cluster.glyphs.iter().map(|glyph| EmojiClusterGlyph {
                        id: glyph.id,
                        x: glyph.x,
                        y: glyph.y,
                        advance: glyph.advance,
                    }));
                });
            }

            if glyphs.is_empty() {
                continue;
            }
            if text.contains('\u{200d}') && glyphs.len() != 1 {
                continue;
            }
            let has_color = glyphs.iter().any(|glyph| {
                self.color_glyph_image_with_size(font_index, glyph.id, self.emoji_size(role))
                    .is_some()
            });
            if !has_color {
                continue;
            }

            let advance = glyphs.iter().map(|glyph| glyph.advance).sum::<f32>();
            return Some(EmojiClusterMetrics {
                font_index,
                glyphs,
                advance: advance.max(self.emoji_size(role)),
            });
        }
        None
    }

    fn emoji_size(&self, role: TextRole) -> f32 {
        self.size(role) * EMOJI_FONT_SCALE
    }

    fn draw_emoji_cluster(
        &self,
        canvas: &mut Canvas,
        metrics: &EmojiClusterMetrics,
        x: f32,
        baseline: f32,
        role: TextRole,
    ) {
        let size = self.emoji_size(role);
        for glyph in &metrics.glyphs {
            if let Some(image) =
                self.color_glyph_image_with_size(metrics.font_index, glyph.id, size)
            {
                draw_color_glyph_image(canvas, x + glyph.x, baseline - glyph.y, &image);
            }
        }
    }
}

fn is_color_emoji_candidate(character: char) -> bool {
    matches!(
        character as u32,
        0x1F000..=0x1FFFF | 0x2600..=0x27BF | 0x2300..=0x23FF
    )
}

fn text_units(text: &str) -> TextUnitIter<'_> {
    TextUnitIter { text, offset: 0 }
}

struct TextUnitIter<'a> {
    text: &'a str,
    offset: usize,
}

impl<'a> Iterator for TextUnitIter<'a> {
    type Item = TextUnit<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.text.len() {
            return None;
        }

        let start = self.offset;
        let mut chars = self.text[start..].char_indices();
        let (_, first) = chars.next()?;
        let mut end = start + first.len_utf8();
        let kind = if is_default_ignorable_for_display(first) {
            TextUnitKind::Ignorable
        } else if is_emoji_cluster_start(first) {
            TextUnitKind::EmojiCluster
        } else {
            TextUnitKind::Character
        };

        if kind == TextUnitKind::EmojiCluster {
            end = consume_emoji_cluster(self.text, end);
        }

        self.offset = end;
        Some(TextUnit {
            text: &self.text[start..end],
            first,
            start,
            end,
            kind,
        })
    }
}

fn consume_emoji_cluster(text: &str, mut offset: usize) -> usize {
    offset = consume_emoji_suffix(text, offset);
    loop {
        let Some(joiner) = char_at(text, offset) else {
            return offset;
        };
        if joiner != '\u{200d}' {
            return offset;
        }
        let after_joiner = offset + joiner.len_utf8();
        let Some(next) = char_at(text, after_joiner) else {
            return offset;
        };
        if !is_emoji_cluster_start(next) {
            return offset;
        }
        offset = consume_emoji_suffix(text, after_joiner + next.len_utf8());
    }
}

fn consume_emoji_suffix(text: &str, mut offset: usize) -> usize {
    while let Some(character) = char_at(text, offset) {
        if is_emoji_modifier(character) || is_variation_selector(character) {
            offset += character.len_utf8();
        } else {
            break;
        }
    }
    offset
}

fn char_at(text: &str, offset: usize) -> Option<char> {
    text.get(offset..)?.chars().next()
}

fn is_emoji_cluster_start(character: char) -> bool {
    is_color_emoji_candidate(character) || matches!(character, '♂' | '♀')
}

fn is_emoji_modifier(character: char) -> bool {
    matches!(character as u32, 0x1F3FB..=0x1F3FF)
}

fn is_variation_selector(character: char) -> bool {
    matches!(character as u32, 0xFE00..=0xFE0F | 0xE0100..=0xE01EF)
}

fn is_default_ignorable_for_display(character: char) -> bool {
    matches!(character, '\u{200d}' | '\u{200c}') || is_variation_selector(character)
}

fn draw_color_glyph_image(
    canvas: &mut Canvas,
    x: f32,
    baseline: f32,
    image: &CachedColorGlyphImage,
) {
    let left = x.floor() as i32 + image.left;
    let top = baseline.floor() as i32 - image.top;
    let width = image.width;
    let height = image.height;
    if width == 0 || height == 0 {
        return;
    }

    for row in 0..height {
        for column in 0..width {
            let index = (row * width + column) * 4;
            if index + 3 >= image.data.len() {
                return;
            }
            canvas.blend_rgba_pixel(
                left + column as i32,
                top + row as i32,
                image.data[index],
                image.data[index + 1],
                image.data[index + 2],
                image.data[index + 3],
            );
        }
    }
}

fn draw_missing_glyph_box(
    canvas: &mut Canvas,
    x: f32,
    baseline: f32,
    advance: f32,
    size: f32,
    color: u32,
) {
    let left = x.round().max(0.0) as usize;
    let top = (baseline - size * 0.85).round().max(0.0) as usize;
    let width = advance.round().max(size * 0.5) as usize;
    let height = (size * 0.85).round().max(1.0) as usize;
    canvas.rect(left, top, width.max(1), height, color);
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
            let bytes = data.to_vec();
            let font_data = Arc::from(bytes.clone().into_boxed_slice());
            FontVec::try_from_vec_and_index(bytes, index)
                .ok()
                .map(FontArc::from)
                .map(|font| LoadedFont {
                    name: name.to_owned(),
                    font,
                    data: font_data,
                    face_index: index as usize,
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
        let start_x = x.min(self.width);
        let end_x = x.saturating_add(width).min(self.width);
        let start_y = y.min(self.height);
        let end_y = y.saturating_add(height).min(self.height);
        if start_x >= end_x || start_y >= end_y {
            return;
        }

        for row in start_y..end_y {
            let row_start = row * self.width;
            self.pixels[row_start + start_x..row_start + end_x].fill(color);
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

    fn set_pixel(&mut self, x: i32, y: i32, color: u32) {
        if x < 0 || y < 0 {
            return;
        }
        let x = x as usize;
        let y = y as usize;
        if x < self.width && y < self.height {
            self.pixels[y * self.width + x] = color;
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
                TextRole::Find => 13.0,
                TextRole::Editor => EDITOR_BASELINE_OFFSET as f32,
            };
        fonts.draw_text(self, x as f32, baseline, text, color, role);
    }

    fn clipped_text(
        &mut self,
        x: usize,
        y: usize,
        text: &str,
        color: u32,
        role: TextRole,
        fonts: &FontStack,
        max_width: usize,
    ) {
        let baseline = y as f32
            + match role {
                TextRole::Ui => 15.0,
                TextRole::Find => 13.0,
                TextRole::Editor => EDITOR_BASELINE_OFFSET as f32,
            };
        fonts.draw_text_clipped(self, x as f32, baseline, text, color, role, max_width);
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

    fn blend_rgba_pixel(&mut self, x: i32, y: i32, red: u8, green: u8, blue: u8, alpha: u8) {
        if x < 0 || y < 0 || alpha == 0 {
            return;
        }
        let x = x as usize;
        let y = y as usize;
        if x >= self.width || y >= self.height {
            return;
        }
        let index = y * self.width + x;
        let color = ((red as u32) << 16) | ((green as u32) << 8) | blue as u32;
        self.pixels[index] = blend(self.pixels[index], color, alpha as f32 / 255.0);
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
    fn gpu_frame_upload_converts_0rgb_pixels_to_rgba() {
        let frame = RenderFrame {
            width: 2,
            height: 1,
            pixels: vec![0x00123456, 0x00abcdef],
        };
        let mut rgba = vec![0; frame.width * frame.height * 4];

        copy_frame_to_rgba(&frame, &mut rgba).expect("convert frame");

        assert_eq!(rgba, vec![0x12, 0x34, 0x56, 0xff, 0xab, 0xcd, 0xef, 0xff]);
    }

    #[test]
    fn gpu_frame_upload_rejects_mismatched_buffer_size() {
        let frame = RenderFrame {
            width: 2,
            height: 1,
            pixels: vec![0x00123456, 0x00abcdef],
        };
        let mut rgba = vec![0; 4];

        assert!(copy_frame_to_rgba(&frame, &mut rgba).is_err());
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
    fn mouse_drag_selection_updates_range_from_anchor() {
        let mut gui_state = GuiState::new().expect("gui state");
        let mut app = BlitzApp::new(EditorSettings::default());
        app.insert_text("abcdef").expect("insert");
        gui_state.selection_anchor = Some(1);
        gui_state.selection_focus = Some(1);
        gui_state.mouse_selecting = true;

        let x = TEXT_MARGIN_X + gui_state.fonts.measure_text("abc", TextRole::Editor) as usize;
        update_mouse_selection(
            &mut app,
            &mut gui_state,
            640,
            400,
            x,
            text_top() + TEXT_MARGIN_Y,
        )
        .expect("drag selection");

        assert_eq!(app.selected_range(), Some(1..3));
        assert_eq!(gui_state.selection_focus, Some(3));

        update_mouse_selection(
            &mut app,
            &mut gui_state,
            640,
            400,
            x,
            text_top() + TEXT_MARGIN_Y,
        )
        .expect("redundant drag selection");
        assert_eq!(app.selected_range(), Some(1..3));
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
    fn text_input_decodes_surrogate_pair_emoji() {
        let queue = Rc::new(RefCell::new(Vec::new()));
        let mut input = TextInput::new(Rc::clone(&queue));

        input.add_char(0xD83D);
        input.add_char(0xDE00);

        assert_eq!(*queue.borrow(), vec!['😀']);
    }

    #[test]
    fn text_input_repairs_legacy_pua_emoji_code_unit() {
        let queue = Rc::new(RefCell::new(Vec::new()));
        let mut input = TextInput::new(Rc::clone(&queue));

        input.add_char(0xF610);

        assert_eq!(*queue.borrow(), vec!['😐']);
    }

    #[test]
    fn pending_text_input_suppresses_command_shortcuts() {
        assert!(should_handle_command_shortcuts(true, false, false));
        assert!(!should_handle_command_shortcuts(true, true, false));
        assert!(!should_handle_command_shortcuts(true, false, true));
        assert!(!should_handle_command_shortcuts(false, false, false));
        assert!(command_shortcuts_suppressed_for_text_input(true));
    }

    #[test]
    fn input_queue_has_text_ignores_control_characters() {
        let queue = Rc::new(RefCell::new(vec!['\u{1}', 'a']));
        assert!(input_queue_has_text(&queue));

        *queue.borrow_mut() = vec!['\u{1}', '\n'];
        assert!(!input_queue_has_text(&queue));
    }

    #[test]
    fn text_units_keep_emoji_zwj_sequence_together() {
        let units = text_units("🙆‍♂️a").collect::<Vec<_>>();

        assert_eq!(units.len(), 2);
        assert_eq!(units[0].text, "🙆‍♂️");
        assert_eq!(units[0].kind, TextUnitKind::EmojiCluster);
        assert_eq!(units[1].text, "a");
    }

    #[test]
    fn font_stack_can_select_emoji_fallback_when_available() {
        let fonts = FontStack::load().expect("fonts");
        let has_emoji_family = fonts.fonts.iter().any(|font| {
            font.name.contains("Emoji")
                || font.name.contains("Symbol")
                || font.name.contains("Segoe Fluent")
        });
        if !has_emoji_family {
            return;
        }

        let grinning = fonts.emoji_cluster_metrics("😀", TextRole::Editor);
        let neutral = fonts.emoji_cluster_metrics("😐", TextRole::Editor);

        assert!(grinning.is_some());
        assert!(neutral.is_some());
    }

    #[test]
    fn emoji_cluster_width_is_close_to_text_height() {
        let fonts = FontStack::load().expect("fonts");
        let Some(metrics) = fonts.emoji_cluster_metrics("🙆‍♂️", TextRole::Editor) else {
            return;
        };

        assert!(metrics.advance <= EDITOR_FONT_SIZE);
        assert!(metrics.advance >= EDITOR_FONT_SIZE * 0.5);
    }

    #[test]
    fn emoji_rendering_produces_color_pixels_when_color_font_is_available() {
        let fonts = FontStack::load().expect("fonts");
        if fonts
            .emoji_cluster_metrics("😀", TextRole::Editor)
            .is_none()
        {
            return;
        }

        let mut app = BlitzApp::new(EditorSettings::default());
        app.insert_text("😀").expect("insert emoji");
        let frame = render_frame(&app, 240, 140).expect("render");

        assert!(frame.pixels.iter().any(|pixel| is_colorful_pixel(*pixel)));
    }

    #[test]
    fn emoji_zwj_sequence_rendering_produces_color_pixels_when_supported() {
        let fonts = FontStack::load().expect("fonts");
        if fonts
            .emoji_cluster_metrics("🙆‍♂️", TextRole::Editor)
            .is_none()
        {
            return;
        }

        let mut app = BlitzApp::new(EditorSettings::default());
        app.insert_text("🙆‍♂️").expect("insert emoji sequence");
        let frame = render_frame(&app, 240, 140).expect("render");

        assert!(frame.pixels.iter().any(|pixel| is_colorful_pixel(*pixel)));
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
    fn font_stack_reuses_color_glyph_images() {
        let fonts = FontStack::load().expect("fonts");
        let Some(metrics) = fonts.emoji_cluster_metrics("😀", TextRole::Editor) else {
            return;
        };
        let Some(glyph) = metrics.glyphs.first() else {
            return;
        };
        fonts.color_glyph_cache.borrow_mut().clear();

        let first = fonts.color_glyph_image_with_size(
            metrics.font_index,
            glyph.id,
            fonts.emoji_size(TextRole::Editor),
        );
        let cache_len = fonts.color_glyph_cache.borrow().len();
        let second = fonts.color_glyph_image_with_size(
            metrics.font_index,
            glyph.id,
            fonts.emoji_size(TextRole::Editor),
        );

        assert!(first.is_some());
        assert!(second.is_some());
        assert_eq!(fonts.color_glyph_cache.borrow().len(), cache_len);
        assert_eq!(cache_len, 1);
    }

    #[test]
    fn font_stack_reuses_emoji_cluster_metrics() {
        let fonts = FontStack::load().expect("fonts");
        fonts.emoji_cluster_cache.borrow_mut().clear();

        let first = fonts.emoji_cluster_metrics("😀", TextRole::Editor);
        let cache_len = fonts
            .emoji_cluster_cache
            .borrow()
            .get(&TextRole::Editor)
            .map(HashMap::len)
            .unwrap_or(0);
        let second = fonts.emoji_cluster_metrics("😀", TextRole::Editor);

        assert_eq!(first.is_some(), second.is_some());
        assert_eq!(cache_len, 1);
        assert_eq!(
            fonts
                .emoji_cluster_cache
                .borrow()
                .get(&TextRole::Editor)
                .map(HashMap::len)
                .unwrap_or(0),
            cache_len
        );
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
    fn frame_signature_tracks_status_message_changes() {
        let app = BlitzApp::new(EditorSettings::default());
        let mut gui_state = GuiState::new().expect("gui state");
        let state = app.ui_state().expect("ui state");
        let initial = frame_signature(&app, &state, 640, 400, &gui_state);

        gui_state.set_message("Printing...");
        let updated = frame_signature(&app, &state, 640, 400, &gui_state);

        assert_ne!(initial, updated);
    }

    #[test]
    fn frame_signature_ignores_status_message_when_status_bar_hidden() {
        let mut app = BlitzApp::new(EditorSettings::default());
        let mut gui_state = GuiState::new().expect("gui state");
        app.toggle_status_bar();
        let state = app.ui_state().expect("ui state");
        let initial = frame_signature(&app, &state, 640, 400, &gui_state);

        gui_state.set_message("Printing...");
        let updated = frame_signature(&app, &state, 640, 400, &gui_state);

        assert_eq!(initial, updated);
    }

    #[test]
    fn status_bar_cells_show_printing_while_print_job_is_active() {
        let app = BlitzApp::new(EditorSettings::default());
        let mut gui_state = GuiState::new().expect("gui state");
        let (_sender, receiver) = mpsc::channel();
        gui_state.print_job = Some(PrintJob { receiver });
        gui_state.set_message("Some older status");

        let cells = status_bar_cells(&app.ui_state().expect("ui state"), &gui_state);

        assert_eq!(cells.first().map(String::as_str), Some("Printing..."));
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
    fn print_request_snapshots_clean_existing_file_contents() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("print.txt");
        std::fs::write(&path, "print me").expect("write");
        let app = BlitzApp::open(&path, EditorSettings::default()).expect("open");

        let request = print_request_for_app(&app).expect("print request");

        assert_eq!(request.title, "print.txt");
        assert_eq!(request.document.text_lossy(), "print me");
    }

    #[test]
    fn print_request_snapshots_dirty_or_untitled_documents() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.insert_text("unsaved print").expect("insert");

        let request = print_request_for_app(&app).expect("print request");

        assert_eq!(request.title, "Untitled");
        assert_eq!(request.document.text_lossy(), "unsaved print");
    }

    #[test]
    fn print_request_uses_lightweight_snapshot_for_large_documents() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.insert_text(&"x".repeat(PRINT_LINE_CHUNK_BYTES + 1))
            .expect("insert");

        let request = print_request_for_app(&app).expect("print request");

        assert_eq!(request.document.len(), PRINT_LINE_CHUNK_BYTES + 1);
        assert!(!request.document.can_undo());
    }

    #[test]
    fn print_line_visitor_splits_crlf_lines_and_long_rows() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.insert_text("a\r\nb\n").expect("insert");
        app.insert_text(&"x".repeat(PRINT_LINE_CHUNK_BYTES + 1))
            .expect("long insert");
        let mut lines = Vec::new();

        visit_print_lines(app.document(), |line| {
            lines.push(line.to_owned());
            Ok(())
        })
        .expect("visit lines");

        assert_eq!(lines[0], "a");
        assert_eq!(lines[1], "b");
        assert!(lines
            .iter()
            .any(|line| line.len() == PRINT_LINE_CHUNK_BYTES));
    }

    #[test]
    fn print_line_visitor_keeps_utf8_scalars_intact_when_splitting_long_rows() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.insert_text(&"x".repeat(PRINT_LINE_CHUNK_BYTES - 1))
            .expect("insert prefix");
        app.insert_text("😀tail").expect("insert emoji");
        let mut lines = Vec::new();

        visit_print_lines(app.document(), |line| {
            lines.push(line.to_owned());
            Ok(())
        })
        .expect("visit lines");

        assert!(lines.iter().any(|line| line.contains("😀tail")));
        assert!(lines.iter().all(|line| !line.contains('\u{fffd}')));
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
    fn selected_text_is_highlighted_in_editor() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.insert_text("abcdef").expect("insert");
        app.set_selection_range(1, 4).expect("selection");

        let frame = render_frame(&app, 640, 400).expect("render");

        assert!(frame.pixels.iter().any(|pixel| *pixel == COLOR_SELECTION));
    }

    #[test]
    fn find_window_view_draws_notepad_style_controls() {
        let fonts = FontStack::load().expect("fonts");
        let view = FindWindowView {
            query: "test".to_owned(),
            match_case: false,
            wrap_around: false,
            forward: true,
        };

        let frame = render_find_window_view(&view, &fonts);

        assert_eq!(frame.width, FIND_WINDOW_WIDTH);
        assert_eq!(frame.height, FIND_WINDOW_HEIGHT);
        assert!(fonts.measure("Find Next", TextRole::Find) <= FIND_NEXT_BUTTON_RECT.width - 8);
        assert_eq!(FIND_FIELD_RECT.y, FIND_NEXT_BUTTON_RECT.y);
        assert_eq!(FIND_FIELD_RECT.height, FIND_NEXT_BUTTON_RECT.height);
        assert_eq!(FIND_NEXT_BUTTON_RECT.x, FIND_CANCEL_BUTTON_RECT.x);
        assert_eq!(FIND_NEXT_BUTTON_RECT.width, FIND_CANCEL_BUTTON_RECT.width);
        assert!(
            FIND_CANCEL_BUTTON_RECT.y + FIND_CANCEL_BUTTON_RECT.height
                <= FIND_DIRECTION_GROUP_RECT.y
        );
        assert!(
            FIND_CANCEL_BUTTON_RECT.y + FIND_CANCEL_BUTTON_RECT.height <= FIND_MATCH_CASE_RECT.y
        );
        assert!(FIND_WRAP_RECT.y + FIND_WRAP_RECT.height <= FIND_WINDOW_HEIGHT);
        assert!(
            FIND_DIRECTION_GROUP_RECT.y + FIND_DIRECTION_GROUP_RECT.height <= FIND_WINDOW_HEIGHT
        );
        assert!(
            FIND_DOWN_RADIO_RECT.x + FIND_DOWN_RADIO_RECT.width
                <= FIND_DIRECTION_GROUP_RECT.x + FIND_DIRECTION_GROUP_RECT.width
        );
        assert!(
            FIND_DIRECTION_GROUP_RECT.x + FIND_DIRECTION_GROUP_RECT.width
                < FIND_CANCEL_BUTTON_RECT.x
        );
        assert!(FIND_UP_RADIO_RECT.x >= FIND_DIRECTION_GROUP_RECT.x);
        assert!(FIND_UP_RADIO_RECT.x + FIND_UP_RADIO_RECT.width <= FIND_DOWN_RADIO_RECT.x);
        assert_eq!(
            frame.pixels[pixel_index(&frame, FIND_FIELD_RECT.x, FIND_FIELD_RECT.y)],
            COLOR_FOCUS_BORDER
        );
        assert_eq!(
            frame.pixels[pixel_index(
                &frame,
                FIND_NEXT_BUTTON_RECT.x + 2,
                FIND_NEXT_BUTTON_RECT.y + 2
            )],
            COLOR_BUTTON
        );
        assert_eq!(
            frame.pixels[pixel_index(
                &frame,
                FIND_MATCH_CASE_RECT.x + 8,
                FIND_MATCH_CASE_RECT.y + 11
            )],
            COLOR_TEXT_AREA
        );
    }

    #[test]
    fn go_to_dialog_view_matches_reference_layout() {
        let fonts = FontStack::load().expect("fonts");
        let dialog = DialogState::GoTo {
            line: "1".to_owned(),
            replace_on_input: true,
        };

        let frame = render_dialog_window(&dialog, &fonts);

        assert_eq!(frame.width, GO_TO_WINDOW_WIDTH);
        assert_eq!(frame.height, GO_TO_WINDOW_HEIGHT);
        assert_eq!(frame.pixels[pixel_index(&frame, 0, 0)], COLOR_WINDOW);
        assert_eq!(
            frame.pixels[pixel_index(&frame, GO_TO_FIELD_RECT.x, GO_TO_FIELD_RECT.y)],
            COLOR_FOCUS_BORDER
        );
        assert_eq!(
            frame.pixels[pixel_index(&frame, GO_TO_FIELD_RECT.x + 1, GO_TO_FIELD_RECT.y + 1)],
            COLOR_TEXT_AREA
        );
        let mut selected_pixels = 0;
        for y in GO_TO_FIELD_RECT.y + 3..GO_TO_FIELD_RECT.y + GO_TO_FIELD_RECT.height - 3 {
            for x in GO_TO_FIELD_RECT.x + 5..GO_TO_FIELD_RECT.x + 20 {
                if frame.pixels[pixel_index(&frame, x, y)] == COLOR_FOCUS_BORDER {
                    selected_pixels += 1;
                }
            }
        }
        assert!(selected_pixels > 0);
        assert_eq!(
            frame.pixels[pixel_index(
                &frame,
                GO_TO_ACCEPT_BUTTON_RECT.x - 1,
                GO_TO_ACCEPT_BUTTON_RECT.y - 1
            )],
            COLOR_FOCUS_BORDER
        );
        assert_eq!(
            frame.pixels[pixel_index(
                &frame,
                GO_TO_ACCEPT_BUTTON_RECT.x + 2,
                GO_TO_ACCEPT_BUTTON_RECT.y + 2
            )],
            COLOR_BUTTON
        );
        assert_eq!(
            frame.pixels[pixel_index(
                &frame,
                GO_TO_CANCEL_BUTTON_RECT.x + 2,
                GO_TO_CANCEL_BUTTON_RECT.y + 2
            )],
            COLOR_BUTTON
        );
        assert!(fonts.measure("Go To", TextRole::Find) <= GO_TO_ACCEPT_BUTTON_RECT.width - 8);
        assert!(fonts.measure("Cancel", TextRole::Find) <= GO_TO_CANCEL_BUTTON_RECT.width - 8);
        assert!(GO_TO_FIELD_RECT.y + GO_TO_FIELD_RECT.height < GO_TO_ACCEPT_BUTTON_RECT.y);
        assert!(
            GO_TO_ACCEPT_BUTTON_RECT.x + GO_TO_ACCEPT_BUTTON_RECT.width
                < GO_TO_CANCEL_BUTTON_RECT.x
        );
    }

    #[test]
    fn go_to_dialog_line_uses_current_caret_line() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.insert_text("one\ntwo\nthree").expect("insert");
        app.set_caret_offset("one\nt".len()).expect("caret");

        assert_eq!(go_to_dialog_line(&app).expect("line"), "2");
    }

    #[test]
    fn go_to_dialog_replaces_initial_line_number_on_first_digit() {
        let mut dialog = DialogState::GoTo {
            line: "1".to_owned(),
            replace_on_input: true,
        };

        append_dialog_character(&mut dialog, '2');
        append_dialog_character(&mut dialog, '5');

        assert_eq!(
            dialog,
            DialogState::GoTo {
                line: "25".to_owned(),
                replace_on_input: false,
            }
        );
    }

    #[test]
    fn go_to_button_action_moves_caret_and_closes_dialog() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.insert_text("one\ntwo\nthree").expect("insert");
        let mut last_search = None;
        let mut pending_reveal_selection = false;
        let mut dialog = DialogState::GoTo {
            line: "2".to_owned(),
            replace_on_input: false,
        };

        let keep_open = activate_go_to_button(
            &mut app,
            &mut last_search,
            &mut pending_reveal_selection,
            &mut dialog,
        )
        .expect("go to action");

        assert!(!keep_open);
        assert_eq!(app.ui_state().expect("ui").caret_line, 2);
        assert!(last_search.is_none());
        assert!(!pending_reveal_selection);
    }

    #[test]
    fn find_textbox_ascii_key_fallback_maps_printable_keys() {
        assert_eq!(ascii_find_char_for_key(Key::A, false), Some('a'));
        assert_eq!(ascii_find_char_for_key(Key::A, true), Some('A'));
        assert_eq!(ascii_find_char_for_key(Key::Key1, false), Some('1'));
        assert_eq!(ascii_find_char_for_key(Key::Key1, true), Some('!'));
        assert_eq!(ascii_find_char_for_key(Key::Space, false), Some(' '));
        assert_eq!(ascii_find_char_for_key(Key::Enter, false), None);
    }

    #[test]
    fn find_textbox_ascii_fallback_is_disabled_while_ime_is_open() {
        assert!(should_use_find_ascii_fallback(true, false, false));
        assert!(!should_use_find_ascii_fallback(true, false, true));
        assert!(!should_use_find_ascii_fallback(true, true, false));
        assert!(!should_use_find_ascii_fallback(false, false, false));
    }

    #[test]
    fn find_ime_position_tracks_query_caret() {
        let fonts = FontStack::load().expect("fonts");

        let empty_x = find_ime_caret_x(&fonts, "");
        let text_x = find_ime_caret_x(&fonts, "kakikukeko");
        let long_x = find_ime_caret_x(&fonts, &"x".repeat(512));

        assert_eq!(empty_x, FIND_FIELD_RECT.x + 6);
        assert_eq!(
            find_ime_caret_y(),
            centered_text_y(FIND_FIELD_RECT, TextRole::Find)
        );
        assert!(find_ime_caret_y() < FIND_FIELD_RECT.y + FIND_FIELD_RECT.height);
        assert!(text_x > empty_x);
        assert!(long_x <= FIND_FIELD_RECT.x + FIND_FIELD_RECT.width - 4);
    }

    #[test]
    fn editor_ime_position_tracks_editor_caret() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.insert_text("abcdef").expect("insert");
        app.set_caret_offset(3).expect("caret");
        let gui_state = GuiState::new().expect("gui state");

        let (x, y) = editor_ime_caret_point(&app, &gui_state, 640, 400).expect("ime point");

        assert!(x > TEXT_MARGIN_X);
        assert_eq!(y, editor_ime_y(text_top() + TEXT_MARGIN_Y));
    }

    #[test]
    fn editor_ime_position_uses_baseline_in_empty_editor() {
        let app = BlitzApp::new(EditorSettings::default());
        let gui_state = GuiState::new().expect("gui state");

        let (x, y) = editor_ime_caret_point(&app, &gui_state, 640, 400).expect("ime point");

        assert_eq!(x, TEXT_MARGIN_X);
        assert_eq!(y, editor_ime_y(text_top() + TEXT_MARGIN_Y));
    }

    #[test]
    fn editor_ime_position_tracks_wrapped_visual_row() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.toggle_word_wrap();
        app.insert_text(&"abcdef ".repeat(24)).expect("insert");
        let gui_state = GuiState::new().expect("gui state");
        let state = app.ui_state().expect("ui state");
        let metrics = editor_metrics(&state, 220, 220).expect("metrics");
        let rows = editor_visible_lines(&app, 0, metrics, &gui_state, true);
        assert!(rows.len() > 1);
        app.set_caret_offset(rows[1].byte_range.start)
            .expect("caret");

        let (_x, y) = editor_ime_caret_point(&app, &gui_state, 220, 220).expect("ime point");

        assert_eq!(
            y,
            editor_ime_y(metrics.text_top + TEXT_MARGIN_Y + EDITOR_LINE_HEIGHT)
        );
    }

    #[test]
    fn find_textbox_paste_appends_clipboard_text() {
        let mut query = "prefix ".to_owned();
        let mut clipboard_cache = String::new();

        append_find_query_paste(&mut query, &mut clipboard_cache, "日本語 search".to_owned());

        assert_eq!(query, "prefix 日本語 search");
        assert_eq!(clipboard_cache, "日本語 search");
    }

    #[test]
    fn paste_menu_enablement_uses_clipboard_cache() {
        assert!(clipboard_has_text("cached text"));
    }

    #[test]
    fn info_window_is_deferred_while_child_dialog_is_open() {
        let info = DialogState::Info {
            title: "Print".to_owned(),
            message: "Print failed".to_owned(),
        };
        let replace = DialogState::Replace {
            query: "needle".to_owned(),
            replacement: "replacement".to_owned(),
            active_field: ReplaceField::Find,
            match_case: false,
        };

        assert!(should_defer_dialog_window(&info, true));
        assert!(!should_defer_dialog_window(&info, false));
        assert!(!should_defer_dialog_window(&replace, true));
    }

    #[test]
    fn selection_highlight_is_clipped_before_vertical_scrollbar() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.insert_text(&"x".repeat(512)).expect("insert");
        app.select_all();

        let frame = render_frame(&app, 640, 400).expect("render");
        let metrics = editor_metrics(&app.ui_state().expect("ui"), 640, 400).expect("metrics");
        let y = text_top() + TEXT_MARGIN_Y + 4;

        assert_eq!(
            frame.pixels[pixel_index(&frame, metrics.text_width + 1, y)],
            COLOR_SCROLLBAR
        );
        assert_ne!(
            frame.pixels[pixel_index(&frame, metrics.text_width + 1, y)],
            COLOR_SELECTION
        );
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
    fn word_wrap_splits_long_editor_line_into_visible_rows() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.toggle_word_wrap();
        app.insert_text(&"abcdef ".repeat(24)).expect("insert");
        let gui_state = GuiState::new().expect("gui state");
        let state = app.ui_state().expect("ui state");
        let metrics = editor_metrics(&state, 220, 220).expect("metrics");

        let rows = editor_visible_lines(&app, 0, metrics, &gui_state, true);

        assert!(rows.len() > 1);
        assert_eq!(rows[0].number, 1);
        assert_eq!(rows[1].number, 1);
        assert_eq!(rows[0].byte_range.end, rows[1].byte_range.start);
        assert!(
            gui_state.fonts.measure(&rows[0].text, TextRole::Editor) <= metrics.editor_text_width
        );
    }

    #[test]
    fn word_wrap_keeps_multibyte_and_emoji_boundaries_intact() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.toggle_word_wrap();
        app.insert_text(&"日本語🙆‍♂️😀".repeat(12))
            .expect("insert multilingual text");
        let gui_state = GuiState::new().expect("gui state");
        let state = app.ui_state().expect("ui state");
        let metrics = editor_metrics(&state, 240, 260).expect("metrics");
        let document_text = app.document().text_lossy();

        let rows = editor_visible_lines(&app, 0, metrics, &gui_state, true);

        assert!(rows.len() > 1);
        assert!(rows.iter().all(|row| !row.text.contains('\u{fffd}')));
        assert!(rows
            .iter()
            .all(|row| document_text.is_char_boundary(row.byte_range.start)));
        assert!(rows
            .iter()
            .all(|row| document_text.is_char_boundary(row.byte_range.end)));
    }

    #[test]
    fn word_wrap_hit_testing_uses_wrapped_visual_row() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.toggle_word_wrap();
        app.insert_text(&"abcdef ".repeat(24)).expect("insert");
        let gui_state = GuiState::new().expect("gui state");
        let state = app.ui_state().expect("ui state");
        let metrics = editor_metrics(&state, 220, 220).expect("metrics");
        let rows = editor_visible_lines(&app, 0, metrics, &gui_state, true);
        assert!(rows.len() > 1);

        let offset = text_offset_for_point(
            &app,
            &gui_state,
            220,
            220,
            TEXT_MARGIN_X,
            metrics.text_top + TEXT_MARGIN_Y + EDITOR_LINE_HEIGHT,
        )
        .expect("offset");

        assert!(rows[1].byte_range.contains(&offset) || offset == rows[1].byte_range.end);
    }

    #[test]
    fn word_wrap_boundary_offsets_belong_to_following_visual_row() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.toggle_word_wrap();
        app.insert_text(&"abcdef ".repeat(24)).expect("insert");
        let gui_state = GuiState::new().expect("gui state");
        let state = app.ui_state().expect("ui state");
        let metrics = editor_metrics(&state, 220, 220).expect("metrics");
        let rows = editor_visible_lines(&app, 0, metrics, &gui_state, true);
        assert!(rows.len() > 1);

        let boundary = rows[1].byte_range.start;
        let visual_row =
            wrapped_visual_row_for_offset(&app, &gui_state, metrics, boundary).expect("row");

        assert_eq!(visual_row, 1);
    }

    #[test]
    fn caret_visual_line_prefers_following_wrapped_row_at_boundary() {
        let rows = vec![
            VisibleLine {
                number: 1,
                byte_range: 0..10,
                text: "abcdefghij".to_owned(),
            },
            VisibleLine {
                number: 1,
                byte_range: 10..20,
                text: "klmnopqrst".to_owned(),
            },
        ];

        assert_eq!(visible_line_index_for_caret(&rows, 1, 10), Some(1));
        assert_eq!(visible_line_index_for_caret(&rows, 1, 20), Some(1));
    }

    #[test]
    fn word_wrap_vertical_scroll_moves_within_one_long_physical_line() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.toggle_word_wrap();
        app.insert_text(&"abcdef ".repeat(96)).expect("insert");
        let mut gui_state = GuiState::new().expect("gui state");
        let state = app.ui_state().expect("ui state");
        let metrics = editor_metrics(&state, 220, 180).expect("metrics");

        let total_rows = vertical_content_len(&app, &gui_state, metrics);
        let before = editor_visible_lines(&app, 0, metrics, &gui_state, true);
        gui_state.scroll_vertical(&app, metrics.visible_line_count as isize, metrics);
        let after = editor_visible_lines(
            &app,
            gui_state.first_visible_line,
            metrics,
            &gui_state,
            true,
        );

        assert_eq!(app.document().line_count(), 1);
        assert!(total_rows > metrics.visible_line_count);
        assert!(gui_state.first_visible_line > 0);
        assert_eq!(after[0].number, 1);
        assert!(after[0].byte_range.start > before[0].byte_range.start);
    }

    #[test]
    fn word_wrap_vertical_scroll_reaches_beyond_first_long_line_chunk() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.toggle_word_wrap();
        app.insert_text(&"abcdef ".repeat(8_000)).expect("insert");
        let mut gui_state = GuiState::new().expect("gui state");
        let state = app.ui_state().expect("ui state");
        let metrics = editor_metrics(&state, 220, 180).expect("metrics");
        let total_rows = vertical_content_len(&app, &gui_state, metrics);

        gui_state.scroll_vertical(&app, total_rows as isize, metrics);
        let rows = editor_visible_lines(
            &app,
            gui_state.first_visible_line,
            metrics,
            &gui_state,
            true,
        );

        assert_eq!(app.document().line_count(), 1);
        assert!(total_rows > metrics.visible_line_count);
        assert!(rows[0].byte_range.start > 16 * 1024);
        assert_eq!(rows[0].number, 1);
    }

    #[test]
    fn reset_scroll_clears_wrapped_row_cache_for_new_one_line_document() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.toggle_word_wrap();
        app.insert_text("short").expect("insert short");
        let mut gui_state = GuiState::new().expect("gui state");
        let state = app.ui_state().expect("ui state");
        let metrics = editor_metrics(&state, 220, 180).expect("metrics");
        assert_eq!(vertical_content_len(&app, &gui_state, metrics), 1);

        app.new_document();
        app.insert_text(&"abcdef ".repeat(96)).expect("insert long");
        gui_state.reset_scroll(&app);
        let state = app.ui_state().expect("ui state");
        let metrics = editor_metrics(&state, 220, 180).expect("metrics");

        assert!(vertical_content_len(&app, &gui_state, metrics) > metrics.visible_line_count);
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
            wrap_around: true,
        });

        find_again(&mut app, &mut gui_state, true).expect("find");
        assert_eq!(app.selected_range(), Some(0..3));
        find_again(&mut app, &mut gui_state, true).expect("find");
        assert_eq!(app.selected_range(), Some(8..11));
    }

    #[test]
    fn find_next_reveals_match_horizontally() {
        let mut app = BlitzApp::new(EditorSettings::default());
        let mut gui_state = GuiState::new().expect("gui state");
        let prefix = "a".repeat(200);
        app.insert_text(&format!("{prefix}needle")).expect("insert");
        gui_state.last_search = Some(SearchSpec {
            query: "needle".to_owned(),
            match_case: true,
            wrap_around: true,
        });

        find_again(&mut app, &mut gui_state, true).expect("find");
        gui_state
            .follow_caret_if_moved(&app, 640, 400)
            .expect("reveal");

        let selection = app.selected_range().expect("selection");
        let metrics = editor_metrics(&app.ui_state().expect("ui"), 640, 400).expect("metrics");
        let content_range = app
            .document()
            .line_content_range_for_offset(selection.start)
            .expect("line range");
        let visible_bytes = visible_byte_capacity(&gui_state.fonts, metrics.editor_text_width);
        let relative_start = selection.start - content_range.start;
        let relative_end = selection.end - content_range.start;

        assert!(gui_state.horizontal_offset > 0);
        assert_eq!(
            gui_state.horizontal_offset,
            centered_offset_for_range(relative_start, relative_end, visible_bytes)
        );
        assert!(gui_state.horizontal_offset <= relative_start);
        assert!(relative_end <= gui_state.horizontal_offset + visible_bytes);
    }

    #[test]
    fn find_previous_reveals_match_when_caret_offset_does_not_change() {
        let mut app = BlitzApp::new(EditorSettings::default());
        let mut gui_state = GuiState::new().expect("gui state");
        let prefix = "a".repeat(200);
        let text = format!("{prefix}needle");
        app.insert_text(&text).expect("insert");
        app.set_caret_offset(text.len()).expect("caret");
        gui_state.last_caret_offset = app.caret_offset();
        gui_state.last_search = Some(SearchSpec {
            query: "needle".to_owned(),
            match_case: true,
            wrap_around: false,
        });

        find_again(&mut app, &mut gui_state, false).expect("find previous");
        assert_eq!(app.caret_offset(), text.len());
        assert!(gui_state.pending_reveal_selection);

        gui_state
            .follow_caret_if_moved(&app, 640, 400)
            .expect("reveal");

        assert!(!gui_state.pending_reveal_selection);
        assert!(gui_state.horizontal_offset > 0);
    }

    #[test]
    fn replace_dialog_can_replace_all() {
        let mut app = BlitzApp::new(EditorSettings::default());
        app.insert_text("one one").expect("insert");
        let mut last_search = None;
        let mut pending_reveal_selection = false;
        let mut dialog = DialogState::Replace {
            query: "one".to_owned(),
            replacement: "two".to_owned(),
            active_field: ReplaceField::Find,
            match_case: true,
        };

        assert!(accept_dialog_state(
            &mut app,
            &mut last_search,
            &mut pending_reveal_selection,
            &mut dialog,
            true,
        )
        .expect("replace all"));
        assert_eq!(app.document().text_lossy(), "two two");
    }

    fn pixel_index(frame: &RenderFrame, x: usize, y: usize) -> usize {
        y * frame.width + x
    }

    fn is_colorful_pixel(pixel: u32) -> bool {
        let red = (pixel >> 16) & 0xff;
        let green = (pixel >> 8) & 0xff;
        let blue = pixel & 0xff;
        red.abs_diff(green) > 24 || red.abs_diff(blue) > 24 || green.abs_diff(blue) > 24
    }
}
