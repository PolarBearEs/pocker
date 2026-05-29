use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs;
use std::io::{self, IsTerminal};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anstyle::{AnsiColor, Style};
use ratatui::backend::CrosstermBackend;
#[cfg(test)]
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style as TuiStyle};
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::units::format_units;

pub(crate) const GREEN: Style = AnsiColor::Green.on_default();
pub(crate) const YELLOW: Style = AnsiColor::Yellow.on_default();
pub(crate) const CYAN: Style = AnsiColor::Cyan.on_default();
pub(crate) const DIM: Style = Style::new().dimmed();

// Rendering is capped to protect slow terminals, remote shells, and constrained
// devices from spending more work repainting than pulling while still waking
// immediately for meaningful phase/status changes.
const MAX_RENDER_FPS: u64 = 8;
const FRAME_INTERVAL: Duration = Duration::from_millis(1_000 / MAX_RENDER_FPS);
const BAR_WIDTH: usize = 22;
const SPINNER: [char; 4] = ['-', '\\', '|', '/'];

pub(crate) fn paint(value: &str, style: Style) -> String {
    if should_color_stderr() {
        format!("{style}{value}{style:#}")
    } else {
        value.to_string()
    }
}

#[derive(Clone)]
pub struct Ui {
    mode: UiMode,
}

#[derive(Clone)]
pub struct UiGroup {
    mode: UiGroupMode,
}

pub trait ProgressSink: Send + Sync {
    fn begin_image(&self, image: &str);
    fn begin_load(&self, image: &str);
    fn set_image_status(&self, image: &str, status: &str);
    fn finish_image(&self, image: &str, status: &str);
    fn prepare_layers(&self, digests: &[String]);
    fn mark_layer_cached(&self, digest: &str);
    fn mark_layer_daemon(&self, digest: &str);
    fn start_layer_download(&self, digest: &str, total_bytes: u64, starting_offset: u64);
    fn advance_layer_download(&self, digest: &str, amount: u64);
    fn finish_layer_download(&self, digest: &str);
    fn set_layer_status(&self, digest: &str, status: &str);
    fn warn(&self, message: &str);
}

#[derive(Clone)]
enum UiMode {
    Quiet,
    Progress {
        renderer: Arc<ProgressRenderer>,
        image_key: String,
    },
    Plain(Arc<PlainUi>),
}

#[derive(Clone)]
enum UiGroupMode {
    Quiet,
    Progress { renderer: Arc<ProgressRenderer> },
    Plain,
}

struct PlainUi {
    prefix: Option<String>,
    state: Mutex<PlainState>,
}

#[derive(Default)]
struct PlainState {
    cached: usize,
    daemon: usize,
}

struct ProgressRenderer {
    shared: Arc<RendererShared>,
    handle: Mutex<Option<thread::JoinHandle<()>>>,
}

struct RendererShared {
    state: Mutex<RendererState>,
    wake: Condvar,
}

struct RendererState {
    model: ProgressModel,
    dirty: bool,
    shutdown: bool,
    immediate: bool,
    frame: usize,
}

#[derive(Default, Clone)]
struct ProgressModel {
    image_order: Vec<String>,
    images: HashMap<String, ImageProgress>,
    warnings: Vec<String>,
}

#[derive(Clone)]
struct ImageProgress {
    display: String,
    phase: ImagePhase,
    layer_order: Vec<String>,
    layers: HashMap<String, LayerProgress>,
    failed: bool,
}

impl ImageProgress {
    fn new(display: String) -> Self {
        Self {
            display,
            phase: ImagePhase::Pulling,
            layer_order: Vec::new(),
            layers: HashMap::new(),
            failed: false,
        }
    }
}

#[derive(Clone)]
enum ImagePhase {
    Pulling,
    Loading,
    Pruning,
    Ready(String),
    AlreadyExists,
    Failed(String),
    Status(String),
}

#[derive(Clone)]
struct LayerProgress {
    digest: String,
    status: LayerStatus,
    total: u64,
    current: u64,
    started_at: Option<Instant>,
    last_rate_at: Instant,
    last_rate_bytes: u64,
    bytes_per_second: Option<u64>,
}

impl LayerProgress {
    fn waiting(digest: String) -> Self {
        let now = Instant::now();
        Self {
            digest,
            status: LayerStatus::Waiting,
            total: 0,
            current: 0,
            started_at: None,
            last_rate_at: now,
            last_rate_bytes: 0,
            bytes_per_second: None,
        }
    }

    fn is_complete(&self) -> bool {
        matches!(
            self.status,
            LayerStatus::Cached | LayerStatus::Daemon | LayerStatus::Complete
        )
    }
}

#[derive(Clone, PartialEq, Eq)]
enum LayerStatus {
    Waiting,
    Cached,
    Daemon,
    Downloading,
    Verifying,
    Complete,
    Retrying(String),
    Status(String),
}

impl Ui {
    pub fn new(quiet: bool, animated: bool) -> Self {
        UiGroup::new(quiet, animated).image_ui("")
    }

    pub fn begin_image(&self, image: &str) {
        match &self.mode {
            UiMode::Progress {
                renderer,
                image_key,
            } => renderer.update_image(image_key, true, |progress| {
                progress.display = image.to_string();
                progress.phase = ImagePhase::Pulling;
                progress.failed = false;
            }),
            UiMode::Plain(plain) => {
                plain.flush_layer_hits();
                plain.line(format!("image {image}: Pulling"));
            }
            UiMode::Quiet => {}
        }
    }

    pub fn begin_load(&self, image: &str) {
        match &self.mode {
            UiMode::Progress {
                renderer,
                image_key,
            } => renderer.update_image(image_key, true, |progress| {
                progress.display = image.to_string();
                progress.phase = ImagePhase::Loading;
            }),
            UiMode::Plain(plain) => {
                plain.flush_layer_hits();
                plain.line(format!("image {image}: Loading"));
            }
            UiMode::Quiet => {}
        }
    }

    pub fn set_image_status(&self, image: &str, status: &str) {
        match &self.mode {
            UiMode::Progress {
                renderer,
                image_key,
            } => renderer.update_image(image_key, true, |progress| {
                progress.display = image.to_string();
                progress.phase = image_phase_from_status(status);
            }),
            UiMode::Plain(plain) => {
                plain.flush_layer_hits();
                plain.line(format!("image {image}: {status}"));
            }
            UiMode::Quiet => {}
        }
    }

    pub fn finish_image(&self, image: &str, status: &str) {
        match &self.mode {
            UiMode::Progress {
                renderer,
                image_key,
            } => renderer.update_image(image_key, true, |progress| {
                progress.display = image.to_string();
                progress.phase = final_image_phase(status);
                progress.failed = status.eq_ignore_ascii_case("failed");
            }),
            UiMode::Plain(plain) => {
                plain.flush_layer_hits();
                plain.line(format!("image {image}: {status}"));
            }
            UiMode::Quiet => {}
        }
    }

    pub fn prepare_layers(&self, digests: &[String]) {
        if let UiMode::Progress {
            renderer,
            image_key,
        } = &self.mode
        {
            renderer.update_image(image_key, true, |image| {
                image.layer_order.clear();
                image.layers.clear();
                for digest in digests {
                    image.layer_order.push(digest.clone());
                    image
                        .layers
                        .insert(digest.clone(), LayerProgress::waiting(digest.clone()));
                }
            });
        }
    }

    pub fn mark_layer_cached(&self, digest: &str) {
        self.finish_layer_state(digest, LayerStatus::Cached, PlainLayerHit::Cached);
    }

    pub fn mark_layer_daemon(&self, digest: &str) {
        self.finish_layer_state(digest, LayerStatus::Daemon, PlainLayerHit::Daemon);
    }

    pub fn start_layer_download(&self, digest: &str, total_bytes: u64, starting_offset: u64) {
        match &self.mode {
            UiMode::Progress {
                renderer,
                image_key,
            } => renderer.update_layer(image_key, digest, true, |layer| {
                let now = Instant::now();
                layer.total = total_bytes;
                layer.current = starting_offset.min(total_bytes);
                layer.status = LayerStatus::Downloading;
                layer.started_at = Some(now);
                layer.last_rate_at = now;
                layer.last_rate_bytes = layer.current;
                layer.bytes_per_second = None;
            }),
            UiMode::Plain(plain) => {
                plain.flush_layer_hits();
                plain.line(plain_layer_download_message(
                    digest,
                    total_bytes,
                    starting_offset,
                ));
            }
            UiMode::Quiet => {}
        }
    }

    pub fn advance_layer_download(&self, digest: &str, amount: u64) {
        if let UiMode::Progress {
            renderer,
            image_key,
        } = &self.mode
        {
            renderer.update_layer(image_key, digest, false, |layer| {
                let now = Instant::now();
                if matches!(layer.status, LayerStatus::Retrying(_)) {
                    layer.status = LayerStatus::Downloading;
                }
                layer.current = layer.current.saturating_add(amount);
                if layer.total > 0 {
                    layer.current = layer.current.min(layer.total);
                }
                let elapsed = now.saturating_duration_since(layer.last_rate_at);
                if elapsed >= Duration::from_millis(500) {
                    let delta = layer.current.saturating_sub(layer.last_rate_bytes);
                    let seconds = elapsed.as_secs_f64();
                    if seconds > 0.0 {
                        layer.bytes_per_second = Some((delta as f64 / seconds) as u64);
                    }
                    layer.last_rate_at = now;
                    layer.last_rate_bytes = layer.current;
                }
            });
        }
    }

    pub fn finish_layer_download(&self, digest: &str) {
        self.finish_layer_state(digest, LayerStatus::Complete, PlainLayerHit::None);
    }

    pub fn set_layer_status(&self, digest: &str, status: &str) {
        match &self.mode {
            UiMode::Progress {
                renderer,
                image_key,
            } => renderer.update_layer(image_key, digest, true, |layer| {
                layer.status = layer_status_from_text(status);
            }),
            UiMode::Plain(plain) => {
                if should_emit_plain_layer_status(status) {
                    plain.flush_layer_hits();
                    plain.line(format!("layer {}: {status}", short_digest(digest)));
                }
            }
            UiMode::Quiet => {}
        }
    }

    fn finish_layer_state(&self, digest: &str, status: LayerStatus, plain_hit: PlainLayerHit) {
        match &self.mode {
            UiMode::Progress {
                renderer,
                image_key,
            } => renderer.update_layer(image_key, digest, true, |layer| {
                if layer.total > 0 {
                    layer.current = layer.total;
                }
                layer.status = status;
                layer.bytes_per_second = None;
            }),
            UiMode::Plain(plain) => plain.record_layer_hit(plain_hit),
            UiMode::Quiet => {}
        }
    }
}

impl ProgressSink for Ui {
    fn begin_image(&self, image: &str) {
        Self::begin_image(self, image);
    }

    fn begin_load(&self, image: &str) {
        Self::begin_load(self, image);
    }

    fn set_image_status(&self, image: &str, status: &str) {
        Self::set_image_status(self, image, status);
    }

    fn finish_image(&self, image: &str, status: &str) {
        Self::finish_image(self, image, status);
    }

    fn prepare_layers(&self, digests: &[String]) {
        Self::prepare_layers(self, digests);
    }

    fn mark_layer_cached(&self, digest: &str) {
        Self::mark_layer_cached(self, digest);
    }

    fn mark_layer_daemon(&self, digest: &str) {
        Self::mark_layer_daemon(self, digest);
    }

    fn start_layer_download(&self, digest: &str, total_bytes: u64, starting_offset: u64) {
        Self::start_layer_download(self, digest, total_bytes, starting_offset);
    }

    fn advance_layer_download(&self, digest: &str, amount: u64) {
        Self::advance_layer_download(self, digest, amount);
    }

    fn finish_layer_download(&self, digest: &str) {
        Self::finish_layer_download(self, digest);
    }

    fn set_layer_status(&self, digest: &str, status: &str) {
        Self::set_layer_status(self, digest, status);
    }

    fn warn(&self, message: &str) {
        match &self.mode {
            UiMode::Progress { renderer, .. } => renderer.warn(message),
            UiMode::Plain(plain) => {
                plain.flush_layer_hits();
                plain.line(format!("warning: {message}"));
            }
            UiMode::Quiet => {}
        }
    }
}

impl UiGroup {
    pub fn new(quiet: bool, animated: bool) -> Self {
        if quiet {
            return Self {
                mode: UiGroupMode::Quiet,
            };
        }

        if !should_render_progress() {
            return Self {
                mode: UiGroupMode::Plain,
            };
        }

        Self {
            mode: UiGroupMode::Progress {
                renderer: ProgressRenderer::spawn(animated),
            },
        }
    }

    pub fn image_ui(&self, plain_prefix: impl Into<String>) -> Ui {
        let plain_prefix = plain_prefix.into();
        match &self.mode {
            UiGroupMode::Quiet => Ui {
                mode: UiMode::Quiet,
            },
            UiGroupMode::Plain => Ui {
                mode: UiMode::Plain(Arc::new(PlainUi {
                    prefix: (!plain_prefix.is_empty()).then_some(plain_prefix),
                    state: Mutex::new(PlainState::default()),
                })),
            },
            UiGroupMode::Progress { renderer } => {
                let image_key = renderer.add_image(plain_prefix);
                Ui {
                    mode: UiMode::Progress {
                        renderer: Arc::clone(renderer),
                        image_key,
                    },
                }
            }
        }
    }
}

impl PlainUi {
    fn line(&self, message: impl AsRef<str>) {
        match &self.prefix {
            Some(prefix) => eprintln!("[{prefix}] {}", message.as_ref()),
            None => eprintln!("{}", message.as_ref()),
        }
    }

    fn record_layer_hit(&self, hit: PlainLayerHit) {
        let mut state = self.state.lock().expect("plain ui state poisoned");
        match hit {
            PlainLayerHit::Cached => state.cached += 1,
            PlainLayerHit::Daemon => state.daemon += 1,
            PlainLayerHit::None => {}
        }
    }

    fn flush_layer_hits(&self) {
        let mut state = self.state.lock().expect("plain ui state poisoned");
        let cached = std::mem::take(&mut state.cached);
        let daemon = std::mem::take(&mut state.daemon);
        drop(state);

        if cached > 0 {
            self.line(format!("layers: {cached} already in cache"));
        }
        if daemon > 0 {
            self.line(format!("layers: {daemon} already in Docker daemon"));
        }
    }
}

enum PlainLayerHit {
    None,
    Cached,
    Daemon,
}

impl ProgressRenderer {
    fn spawn(animated: bool) -> Arc<Self> {
        let shared = Arc::new(RendererShared {
            state: Mutex::new(RendererState {
                model: ProgressModel::default(),
                dirty: false,
                shutdown: false,
                immediate: false,
                frame: 0,
            }),
            wake: Condvar::new(),
        });
        let thread_shared = Arc::clone(&shared);
        let handle = thread::spawn(move || render_loop(thread_shared, animated));
        Arc::new(Self {
            shared,
            handle: Mutex::new(Some(handle)),
        })
    }

    fn add_image(&self, display: String) -> String {
        let mut state = self.shared.state.lock().expect("ui state poisoned");
        let key = format!("image-{}", state.model.image_order.len());
        state.model.image_order.push(key.clone());
        state
            .model
            .images
            .insert(key.clone(), ImageProgress::new(display));
        state.dirty = true;
        state.immediate = true;
        self.shared.wake.notify_one();
        key
    }

    fn update_image(
        &self,
        image_key: &str,
        immediate: bool,
        update: impl FnOnce(&mut ImageProgress),
    ) {
        let mut state = self.shared.state.lock().expect("ui state poisoned");
        if let Some(image) = state.model.images.get_mut(image_key) {
            update(image);
            state.dirty = true;
            state.immediate |= immediate;
            self.shared.wake.notify_one();
        }
    }

    fn update_layer(
        &self,
        image_key: &str,
        digest: &str,
        immediate: bool,
        update: impl FnOnce(&mut LayerProgress),
    ) {
        let mut state = self.shared.state.lock().expect("ui state poisoned");
        let Some(image) = state.model.images.get_mut(image_key) else {
            return;
        };
        if !image.layers.contains_key(digest) {
            image.layer_order.push(digest.to_string());
            image.layers.insert(
                digest.to_string(),
                LayerProgress::waiting(digest.to_string()),
            );
        }
        if let Some(layer) = image.layers.get_mut(digest) {
            update(layer);
        }
        state.dirty = true;
        state.immediate |= immediate;
        self.shared.wake.notify_one();
    }

    fn warn(&self, message: &str) {
        let mut state = self.shared.state.lock().expect("ui state poisoned");
        state.model.warnings.push(format!("warning: {message}"));
        state.dirty = true;
        state.immediate = true;
        self.shared.wake.notify_one();
    }
}

impl Drop for ProgressRenderer {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().expect("ui state poisoned");
            state.shutdown = true;
            state.dirty = true;
            state.immediate = true;
            self.shared.wake.notify_one();
        }
        if let Some(handle) = self.handle.lock().expect("renderer handle poisoned").take() {
            let _ = handle.join();
        }
    }
}

fn render_loop(shared: Arc<RendererShared>, animated: bool) {
    let mut last_render = Instant::now() - FRAME_INTERVAL;
    let mut terminal = None;
    let mut terminal_rows = 0_u16;

    loop {
        let mut state = shared.state.lock().expect("ui state poisoned");
        while !state.dirty && !state.shutdown {
            state = shared.wake.wait(state).expect("ui state poisoned");
        }

        let now = Instant::now();
        if !state.shutdown
            && !state.immediate
            && let Some(wait) =
                FRAME_INTERVAL.checked_sub(now.saturating_duration_since(last_render))
        {
            let (next_state, _) = shared
                .wake
                .wait_timeout(state, wait)
                .expect("ui state poisoned");
            state = next_state;
            if !state.dirty && !state.shutdown {
                continue;
            }
        }

        let shutdown = state.shutdown;
        let model = state.model.clone();
        let frame = state.frame;
        state.frame = state.frame.wrapping_add(1);
        state.dirty = false;
        state.immediate = false;
        drop(state);

        let rows = model_height(&model).max(1).min(u16::MAX as usize) as u16;
        if terminal_rows != rows {
            terminal = match inline_terminal(rows) {
                Ok(mut next_terminal) => {
                    let _ = next_terminal.hide_cursor();
                    terminal_rows = rows;
                    Some(next_terminal)
                }
                Err(_) => None,
            };
        }

        if let Some(terminal) = terminal.as_mut() {
            let _ = terminal.draw(|frame_ref| {
                render_model_to_buffer(
                    &model,
                    frame_ref.area(),
                    frame_ref.buffer_mut(),
                    frame,
                    animated,
                );
            });
        }

        last_render = Instant::now();

        if shutdown {
            if let Some(terminal) = terminal.as_mut() {
                let _ = terminal.show_cursor();
            }
            break;
        }
    }
}

fn inline_terminal(rows: u16) -> io::Result<Terminal<CrosstermBackend<io::Stderr>>> {
    Terminal::with_options(
        CrosstermBackend::new(io::stderr()),
        TerminalOptions {
            viewport: Viewport::Inline(rows),
        },
    )
}

#[cfg(test)]
fn render_model_lines(
    model: &ProgressModel,
    width: u16,
    frame: usize,
    animated: bool,
) -> Vec<String> {
    let height = model_height(model).max(1) as u16;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test backend should initialize");
    terminal
        .draw(|frame_ref| {
            render_model_to_buffer(
                model,
                frame_ref.area(),
                frame_ref.buffer_mut(),
                frame,
                animated,
            );
        })
        .expect("test backend should draw");
    buffer_lines(terminal.backend().buffer())
}

fn render_model_to_buffer(
    model: &ProgressModel,
    area: Rect,
    buffer: &mut Buffer,
    frame: usize,
    animated: bool,
) {
    let mut y = area.y;
    for image_key in &model.image_order {
        let Some(image) = model.images.get(image_key) else {
            continue;
        };
        if y >= area.bottom() {
            return;
        }
        let line = image_line(image, frame, animated);
        buffer.set_stringn(
            area.x,
            y,
            truncate_to_width(&line, area.width),
            area.width as usize,
            TuiStyle::default().fg(Color::White),
        );
        y += 1;

        for digest in &image.layer_order {
            let Some(layer) = image.layers.get(digest) else {
                continue;
            };
            if y >= area.bottom() {
                return;
            }
            let line = layer_line(layer, frame, animated);
            buffer.set_stringn(
                area.x,
                y,
                truncate_to_width(&line, area.width),
                area.width as usize,
                layer_style(layer),
            );
            y += 1;
        }
    }

    for warning in &model.warnings {
        if y >= area.bottom() {
            return;
        }
        buffer.set_stringn(
            area.x,
            y,
            truncate_to_width(warning, area.width),
            area.width as usize,
            TuiStyle::default().fg(Color::Yellow),
        );
        y += 1;
    }
}

fn image_line(image: &ImageProgress, frame: usize, animated: bool) -> String {
    let (complete_layers, total_layers) = image_layer_counts(image);
    let (current_bytes, total_bytes) = image_byte_counts(image);
    let phase = image_phase_label(&image.phase);
    let pulse = pulse_suffix(frame, animated);
    let layers = if total_layers > 0 {
        format!("  {complete_layers}/{total_layers} layers")
    } else {
        String::new()
    };
    let bytes = if total_bytes > 0 {
        format!(
            "  {}/{}",
            format_bytes(current_bytes.min(total_bytes)),
            format_bytes(total_bytes)
        )
    } else {
        String::new()
    };
    format!("{}  {phase}{pulse}{layers}{bytes}", image.display)
}

fn layer_line(layer: &LayerProgress, frame: usize, animated: bool) -> String {
    let digest = short_digest(&layer.digest);
    match &layer.status {
        LayerStatus::Waiting => format!("  {digest}  Waiting"),
        LayerStatus::Cached => format!("  {digest}  Already exists"),
        LayerStatus::Daemon => format!("  {digest}  Already exists in Docker daemon"),
        LayerStatus::Downloading => {
            let pulse = pulse_suffix(frame, animated);
            let rate = layer
                .bytes_per_second
                .map(|rate| format!("  {}/s", format_bytes(rate)))
                .unwrap_or_default();
            format!(
                "  {digest}  Downloading{pulse}  [{}]  {}/{}{rate}",
                progress_bar(layer.current, layer.total, BAR_WIDTH),
                format_bytes(layer.current.min(layer.total)),
                format_bytes(layer.total)
            )
        }
        LayerStatus::Verifying => format!("  {digest}  Verifying checksum"),
        LayerStatus::Complete => format!("  {digest}  Pull complete"),
        LayerStatus::Retrying(detail) => format!("  {digest}  Retrying: {detail}"),
        LayerStatus::Status(status) => format!("  {digest}  {status}"),
    }
}

fn layer_style(layer: &LayerProgress) -> TuiStyle {
    match layer.status {
        LayerStatus::Cached | LayerStatus::Daemon | LayerStatus::Complete => {
            TuiStyle::default().fg(Color::Green)
        }
        LayerStatus::Retrying(_) => TuiStyle::default().fg(Color::Yellow),
        _ => TuiStyle::default(),
    }
}

fn image_layer_counts(image: &ImageProgress) -> (usize, usize) {
    let total = image.layer_order.len();
    let complete = image
        .layer_order
        .iter()
        .filter_map(|digest| image.layers.get(digest))
        .filter(|layer| layer.is_complete())
        .count();
    (complete, total)
}

fn image_byte_counts(image: &ImageProgress) -> (u64, u64) {
    image
        .layers
        .values()
        .fold((0_u64, 0_u64), |(current, total), layer| {
            (current + layer.current, total + layer.total)
        })
}

fn image_phase_label(phase: &ImagePhase) -> &str {
    match phase {
        ImagePhase::Pulling => "Pulling",
        ImagePhase::Loading => "Loading",
        ImagePhase::Pruning => "Pruning",
        ImagePhase::Ready(status) => status,
        ImagePhase::AlreadyExists => "Already exists",
        ImagePhase::Failed(status) => status,
        ImagePhase::Status(status) => status,
    }
}

fn image_phase_from_status(status: &str) -> ImagePhase {
    match status {
        "Pruning cache" => ImagePhase::Pruning,
        "Already exists" => ImagePhase::AlreadyExists,
        _ => ImagePhase::Status(status.to_string()),
    }
}

fn final_image_phase(status: &str) -> ImagePhase {
    match status {
        "Ready" | "Pulled" => ImagePhase::Ready(status.to_string()),
        "Already exists" => ImagePhase::AlreadyExists,
        _ if status.to_ascii_lowercase().contains("fail") => ImagePhase::Failed(status.to_string()),
        _ => ImagePhase::Ready(status.to_string()),
    }
}

fn layer_status_from_text(status: &str) -> LayerStatus {
    match status {
        "Verifying checksum" => LayerStatus::Verifying,
        status if status.to_ascii_lowercase().contains("retry") => {
            LayerStatus::Retrying(status.to_string())
        }
        _ => LayerStatus::Status(status.to_string()),
    }
}

fn should_emit_plain_layer_status(status: &str) -> bool {
    status != "Verifying checksum"
}

fn progress_bar(current: u64, total: u64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if total == 0 {
        return " ".repeat(width);
    }
    let filled = ((current.min(total) as u128 * width as u128) / total as u128) as usize;
    let mut bar = String::with_capacity(width);
    for index in 0..width {
        if index < filled {
            bar.push('=');
        } else if index == filled && current < total {
            bar.push('>');
        } else {
            bar.push(' ');
        }
    }
    bar
}

fn pulse_suffix(frame: usize, animated: bool) -> String {
    if animated {
        format!(" {}", SPINNER[frame % SPINNER.len()])
    } else {
        String::new()
    }
}

fn model_height(model: &ProgressModel) -> usize {
    let image_rows = model
        .image_order
        .iter()
        .filter_map(|key| model.images.get(key))
        .map(|image| 1 + image.layer_order.len())
        .sum::<usize>();
    image_rows + model.warnings.len()
}

#[cfg(test)]
fn buffer_lines(buffer: &Buffer) -> Vec<String> {
    let area = buffer.area;
    (area.y..area.bottom())
        .map(|y| {
            let mut line = String::new();
            for x in area.x..area.right() {
                line.push_str(buffer[(x, y)].symbol());
            }
            line.trim_end().to_string()
        })
        .collect()
}

fn truncate_to_width(value: &str, width: u16) -> String {
    let width = width as usize;
    value.chars().take(width).collect()
}

fn should_render_progress() -> bool {
    let stderr = std::io::stderr();
    if !stderr.is_terminal() {
        return false;
    }

    process_is_foreground_tty_job()
}

#[cfg(target_os = "linux")]
fn process_is_foreground_tty_job() -> bool {
    let Ok(stat) = fs::read_to_string("/proc/self/stat") else {
        return true;
    };
    linux_process_is_foreground_tty_job_from_stat(&stat).unwrap_or(true)
}

#[cfg(target_os = "linux")]
fn linux_process_is_foreground_tty_job_from_stat(stat: &str) -> Option<bool> {
    let (_, rest) = stat.rsplit_once(") ")?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() < 6 {
        return None;
    }

    let process_group = fields[2].parse::<i32>().ok()?;
    let tty_nr = fields[4].parse::<i32>().ok()?;
    let foreground_group = fields[5].parse::<i32>().ok()?;

    Some(tty_nr == 0 || process_group == foreground_group)
}

#[cfg(not(target_os = "linux"))]
fn process_is_foreground_tty_job() -> bool {
    true
}

fn short_digest(digest: &str) -> String {
    let value = digest
        .split_once(':')
        .map(|(_, value)| value)
        .unwrap_or(digest);
    value.chars().take(12).collect()
}

fn plain_layer_download_message(digest: &str, total_bytes: u64, starting_offset: u64) -> String {
    if starting_offset == 0 {
        return format!(
            "layer {}: Downloading {}",
            short_digest(digest),
            format_bytes(total_bytes)
        );
    }

    format!(
        "layer {}: Resuming {}/{}",
        short_digest(digest),
        format_bytes(starting_offset),
        format_bytes(total_bytes)
    )
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    format_units(bytes, 1024.0, &UNITS)
}

pub(crate) fn should_color_stderr() -> bool {
    static SHOULD_COLOR_STDERR: OnceLock<bool> = OnceLock::new();

    *SHOULD_COLOR_STDERR
        .get_or_init(|| std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

#[cfg(test)]
mod tests {
    use super::{
        ImagePhase, ImageProgress, LayerProgress, LayerStatus, ProgressModel,
        linux_process_is_foreground_tty_job_from_stat, plain_layer_download_message,
        render_model_lines,
    };

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_foreground_process_group_is_detected() {
        let stat = "1234 (pocker) S 1200 1234 1200 34816 1234 0 0 0 0 0 0 0 0 20 0 1 0 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(
            linux_process_is_foreground_tty_job_from_stat(stat),
            Some(true)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_background_process_group_is_detected() {
        let stat = "1234 (pocker) T 1200 1234 1200 34816 5678 0 0 0 0 0 0 0 0 20 0 1 0 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(
            linux_process_is_foreground_tty_job_from_stat(stat),
            Some(false)
        );
    }

    #[test]
    fn plain_download_message_describes_new_download() {
        assert_eq!(
            plain_layer_download_message("sha256:abcdef0123456789", 1_536, 0),
            "layer abcdef012345: Downloading 1.5 KiB"
        );
    }

    #[test]
    fn plain_download_message_describes_resume_offset() {
        assert_eq!(
            plain_layer_download_message("sha256:abcdef0123456789", 2_048, 512),
            "layer abcdef012345: Resuming 512 B/2.0 KiB"
        );
    }

    #[test]
    fn planned_layers_preserve_row_order() {
        let mut model = model_with_image("docker.io/library/alpine:latest");
        let image = model.images.get_mut("image-0").expect("image exists");
        image.layer_order = vec![
            "sha256:bbbbbbbbbbbb0000".into(),
            "sha256:aaaaaaaaaaaa0000".into(),
        ];
        for digest in image.layer_order.clone() {
            image
                .layers
                .insert(digest.clone(), LayerProgress::waiting(digest));
        }

        let lines = render_model_lines(&model, 100, 0, false);

        assert!(lines[1].contains("bbbbbbbbbbbb"));
        assert!(lines[2].contains("aaaaaaaaaaaa"));
    }

    #[test]
    fn download_advances_coalesce_to_final_position() {
        let mut layer = LayerProgress::waiting("sha256:abcdef0123456789".into());
        layer.status = LayerStatus::Downloading;
        layer.total = 100;
        layer.current = 10;
        layer.current = layer.current.saturating_add(25).min(layer.total);
        layer.current = layer.current.saturating_add(65).min(layer.total);

        assert_eq!(layer.current, 100);
    }

    #[test]
    fn resume_offsets_display_in_downloading_row() {
        let mut model = model_with_image("docker.io/library/alpine:latest");
        insert_layer(
            &mut model,
            "sha256:abcdef0123456789",
            LayerStatus::Downloading,
            512,
            2048,
        );

        let lines = render_model_lines(&model, 100, 0, false);

        assert!(lines[1].contains("512 B/2.0 KiB"));
    }

    #[test]
    fn cached_and_daemon_layers_count_as_complete() {
        let mut model = model_with_image("docker.io/library/alpine:latest");
        insert_layer(
            &mut model,
            "sha256:cached0000000",
            LayerStatus::Cached,
            0,
            0,
        );
        insert_layer(
            &mut model,
            "sha256:daemon0000000",
            LayerStatus::Daemon,
            0,
            0,
        );
        insert_layer(
            &mut model,
            "sha256:download00000",
            LayerStatus::Downloading,
            2,
            10,
        );

        let lines = render_model_lines(&model, 100, 0, false);

        assert!(lines[0].contains("2/3 layers"));
    }

    #[test]
    fn no_animations_disables_pulse_glyphs() {
        let mut model = model_with_image("docker.io/library/alpine:latest");
        insert_layer(
            &mut model,
            "sha256:abcdef0123456789",
            LayerStatus::Downloading,
            512,
            2048,
        );

        let animated = render_model_lines(&model, 100, 2, true);
        let still = render_model_lines(&model, 100, 2, false);

        assert!(animated[1].contains("Downloading |"));
        assert!(still[1].contains("Downloading  ["));
    }

    #[test]
    fn renderer_snapshot_waiting_cached_complete_and_ready() {
        let mut model = model_with_image("docker.io/library/alpine:latest");
        insert_layer(
            &mut model,
            "sha256:aaaabbbbcccc",
            LayerStatus::Waiting,
            0,
            0,
        );
        insert_layer(&mut model, "sha256:ddddeeeeffff", LayerStatus::Cached, 0, 0);
        insert_layer(
            &mut model,
            "sha256:111122223333",
            LayerStatus::Complete,
            1024,
            1024,
        );
        model.images.get_mut("image-0").expect("image exists").phase =
            ImagePhase::Ready("Ready".into());

        let lines = render_model_lines(&model, 100, 0, false);

        assert_eq!(
            lines,
            vec![
                "docker.io/library/alpine:latest  Ready  2/3 layers  1.0 KiB/1.0 KiB",
                "  aaaabbbbcccc  Waiting",
                "  ddddeeeeffff  Already exists",
                "  111122223333  Pull complete",
            ]
        );
    }

    #[test]
    fn renderer_truncates_narrow_width() {
        let mut model = model_with_image("docker.io/library/alpine:latest");
        insert_layer(
            &mut model,
            "sha256:abcdef0123456789",
            LayerStatus::Downloading,
            512,
            2048,
        );

        let lines = render_model_lines(&model, 30, 0, false);

        assert!(lines.iter().all(|line| line.chars().count() <= 30));
        assert_eq!(lines[0], "docker.io/library/alpine:lates");
    }

    fn model_with_image(display: &str) -> ProgressModel {
        let mut model = ProgressModel::default();
        model.image_order.push("image-0".into());
        model
            .images
            .insert("image-0".into(), ImageProgress::new(display.into()));
        model
    }

    fn insert_layer(
        model: &mut ProgressModel,
        digest: &str,
        status: LayerStatus,
        current: u64,
        total: u64,
    ) {
        let image = model.images.get_mut("image-0").expect("image exists");
        image.layer_order.push(digest.into());
        let mut layer = LayerProgress::waiting(digest.into());
        layer.status = status;
        layer.current = current;
        layer.total = total;
        image.layers.insert(digest.into(), layer);
    }
}
