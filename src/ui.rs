use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs;
use std::io::IsTerminal;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anstyle::{AnsiColor, Style};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::units::format_units;

pub(crate) const GREEN: Style = AnsiColor::Green.on_default();
pub(crate) const YELLOW: Style = AnsiColor::Yellow.on_default();
pub(crate) const CYAN: Style = AnsiColor::Cyan.on_default();
pub(crate) const DIM: Style = Style::new().dimmed();

const PROGRESS_REFRESH_HZ: u8 = 12;
// Keep aggregate progress below indicatif's default terminal refresh rate. This
// protects slower terminals from chunk-driven redraw storms while still showing
// sub-second progress during long pulls.
const AGGREGATE_RENDER_INTERVAL: Duration = Duration::from_millis(100);
const DETAIL_RENDER_INTERVAL: Duration = Duration::from_millis(100);

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
    Progress(Arc<ProgressUiInner>),
    Plain { prefix: Option<String> },
}

#[derive(Clone)]
enum UiGroupMode {
    Quiet,
    Progress {
        animated: bool,
        multi: MultiProgress,
    },
    Plain,
}

struct ProgressUiInner {
    animated: bool,
    aggregate_layers: bool,
    multi: MultiProgress,
    image: ProgressBar,
    layers: Mutex<HashMap<String, ProgressBar>>,
    layer_renders: Mutex<HashMap<String, LayerRender>>,
    aggregate: Mutex<AggregateProgress>,
    image_name: Mutex<String>,
}

struct LayerRender {
    total: u64,
    position: u64,
    last_rendered_at: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
enum LayerRenderAdvance {
    Render(u64),
    Throttled,
    Missing,
}

#[derive(Default)]
struct AggregateProgress {
    order: Vec<String>,
    layers: HashMap<String, AggregateLayer>,
    last_rendered: Option<AggregateRender>,
    last_rendered_at: Option<Instant>,
}

#[derive(Default)]
struct AggregateLayer {
    total: u64,
    position: u64,
    state: AggregateLayerState,
    status: Option<String>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum AggregateLayerState {
    #[default]
    Waiting,
    Downloading,
    Complete,
}

#[derive(Clone, PartialEq, Eq)]
struct AggregateRender {
    strip: String,
    total_bytes: u64,
    position: u64,
    hide_bytes: bool,
    completed_layers: usize,
    total_layers: usize,
    status: String,
}

impl AggregateProgress {
    fn clear(&mut self) {
        self.order.clear();
        self.layers.clear();
        self.last_rendered = None;
        self.last_rendered_at = None;
    }

    fn touch_layer(&mut self, digest: &str) -> &mut AggregateLayer {
        if !self.layers.contains_key(digest) {
            self.order.push(digest.to_string());
        }
        self.layers.entry(digest.to_string()).or_default()
    }
}

impl Ui {
    pub fn new(quiet: bool, animated: bool) -> Self {
        if quiet {
            return Self {
                mode: UiMode::Quiet,
            };
        }

        if !should_render_progress() {
            return Self {
                mode: UiMode::Plain { prefix: None },
            };
        }

        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(
            PROGRESS_REFRESH_HZ,
        ));
        let image = multi.add(ProgressBar::new_spinner());
        image.set_style(image_status_style(animated));
        if animated {
            image.enable_steady_tick(Duration::from_millis(100));
        }

        Self {
            mode: UiMode::Progress(Arc::new(ProgressUiInner {
                animated,
                aggregate_layers: false,
                multi,
                image,
                layers: Mutex::new(HashMap::new()),
                layer_renders: Mutex::new(HashMap::new()),
                aggregate: Mutex::new(AggregateProgress::default()),
                image_name: Mutex::new(String::new()),
            })),
        }
    }

    pub fn begin_image(&self, image: &str) {
        self.dispatch_mode(
            |inner| {
                inner.reset_for_image();
                *inner.image_name.lock().expect("ui state poisoned") = image.to_string();
                if inner.aggregate_layers {
                    inner.image.set_style(compose_image_style());
                }
                inner.image.set_message(format!("{image} Pulling"));
            },
            || self.plain_line(format!("image {image}: Pulling")),
        );
    }

    pub fn begin_load(&self, image: &str) {
        self.dispatch_mode(
            |inner| {
                if inner.aggregate_layers {
                    inner.image.set_style(image_status_style(false));
                }
                inner.image.set_message(format!("{image} Loading"));
            },
            || self.plain_line(format!("image {image}: Loading")),
        );
    }

    pub fn set_image_status(&self, image: &str, status: &str) {
        self.dispatch_mode(
            |inner| {
                if inner.aggregate_layers {
                    inner.image.set_style(image_status_style(false));
                }
                inner.image.set_message(format!("{image} {status}"));
            },
            || self.plain_line(format!("image {image}: {status}")),
        );
    }

    pub fn finish_image(&self, image: &str, status: &str) {
        self.dispatch_mode(
            |inner| {
                inner.clear_layers();
                inner.image.disable_steady_tick();
                if inner.aggregate_layers {
                    inner.image.set_style(image_status_style(false));
                }
                inner.image.finish_with_message(format!("{image} {status}"));
            },
            || self.plain_line(format!("image {image}: {status}")),
        );
    }

    pub fn prepare_layers(&self, digests: &[String]) {
        let Some(inner) = self.progress() else {
            return;
        };
        if inner.aggregate_layers {
            inner.prepare_aggregate_layers(digests);
            let mut layers = inner.layers.lock().expect("ui state poisoned");
            let mut after = inner.image.clone();
            for digest in digests {
                let bar = inner.multi.insert_after(&after, ProgressBar::new_spinner());
                bar.set_style(layer_status_style(false));
                bar.set_message(format!("{} Waiting", short_digest(digest)));
                layers.insert(digest.clone(), bar);
                after = layers.get(digest).expect("layer was just inserted").clone();
            }
            return;
        }
        let mut layers = inner.layers.lock().expect("ui state poisoned");
        for digest in digests {
            let bar = inner.multi.add(ProgressBar::new_spinner());
            bar.set_style(layer_status_style(inner.animated));
            if inner.animated {
                bar.enable_steady_tick(Duration::from_millis(120));
            }
            bar.set_message(format!("{} Pulling fs layer", short_digest(digest)));
            layers.insert(digest.clone(), bar);
        }
    }

    pub fn mark_layer_cached(&self, digest: &str) {
        self.finish_layer_status(digest, "Already exists", "Already exists in cache");
    }

    pub fn mark_layer_daemon(&self, digest: &str) {
        self.finish_layer_status(digest, "Already exists", "Already exists in Docker daemon");
    }

    pub fn start_layer_download(&self, digest: &str, total_bytes: u64, starting_offset: u64) {
        self.dispatch_mode(
            |inner| {
                if inner.aggregate_layers {
                    inner.start_aggregate_layer(digest, total_bytes, starting_offset);
                }
                let Some(bar) = inner.layer_bar(digest) else {
                    return;
                };
                bar.set_style(layer_download_style());
                bar.set_length(total_bytes);
                bar.set_position(starting_offset);
                bar.set_message(short_digest(digest));
                inner.start_layer_render(digest, total_bytes, starting_offset);
            },
            || {
                self.plain_line(plain_layer_download_message(
                    digest,
                    total_bytes,
                    starting_offset,
                ));
            },
        );
    }

    pub fn advance_layer_download(&self, digest: &str, amount: u64) {
        self.dispatch_mode(
            |inner| {
                if inner.aggregate_layers {
                    inner.advance_aggregate_layer(digest, amount);
                }
                match inner.advance_layer_render(digest, amount) {
                    LayerRenderAdvance::Render(position) => {
                        if let Some(bar) = inner.layer_bar(digest) {
                            bar.set_position(position);
                        }
                    }
                    LayerRenderAdvance::Missing => {
                        if let Some(bar) = inner.layer_bar(digest) {
                            bar.inc(amount);
                        }
                    }
                    LayerRenderAdvance::Throttled => {}
                }
            },
            || {},
        );
    }

    pub fn finish_layer_download(&self, digest: &str) {
        self.finish_layer_status(digest, "Pull complete", "Pull complete");
    }

    pub fn set_layer_status(&self, digest: &str, status: &str) {
        self.dispatch_mode(
            |inner| {
                if inner.aggregate_layers {
                    inner.set_aggregate_layer_status(digest, status);
                }
                let Some(bar) = inner.layer_bar(digest) else {
                    return;
                };
                if let Some(position) = inner.flush_layer_render(digest) {
                    bar.set_position(position);
                }
                bar.set_style(layer_status_style(inner.animated));
                if inner.animated {
                    bar.enable_steady_tick(Duration::from_millis(120));
                }
                bar.set_message(format!("{} {status}", short_digest(digest)));
            },
            || {
                self.plain_line(format!("layer {}: {status}", short_digest(digest)));
            },
        );
    }

    fn finish_layer_status(&self, digest: &str, progress_status: &str, plain_status: &str) {
        self.dispatch_mode(
            |inner| {
                if inner.aggregate_layers {
                    inner.finish_aggregate_layer(digest, progress_status);
                }
                let Some(bar) = inner.layer_bar(digest) else {
                    return;
                };
                if let Some(position) = inner.finish_layer_render(digest) {
                    bar.set_position(position);
                }
                bar.set_style(layer_status_style(inner.animated));
                bar.disable_steady_tick();
                bar.finish_with_message(format!("{} {progress_status}", short_digest(digest)));
            },
            || {
                self.plain_line(format!("layer {}: {plain_status}", short_digest(digest)));
            },
        );
    }

    fn progress(&self) -> Option<&Arc<ProgressUiInner>> {
        match &self.mode {
            UiMode::Progress(inner) => Some(inner),
            UiMode::Quiet | UiMode::Plain { .. } => None,
        }
    }

    fn dispatch_mode(&self, progress: impl FnOnce(&Arc<ProgressUiInner>), plain: impl FnOnce()) {
        match &self.mode {
            UiMode::Progress(inner) => progress(inner),
            UiMode::Plain { .. } => plain(),
            UiMode::Quiet => {}
        }
    }

    fn plain_line(&self, message: impl AsRef<str>) {
        match &self.mode {
            UiMode::Plain {
                prefix: Some(prefix),
            } => eprintln!("[{prefix}] {}", message.as_ref()),
            UiMode::Plain { prefix: None } => eprintln!("{}", message.as_ref()),
            UiMode::Quiet | UiMode::Progress(_) => {}
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
        let message = format!("warning: {message}");
        match &self.mode {
            UiMode::Progress(inner) => inner.image.println(message),
            UiMode::Plain { .. } => self.plain_line(message),
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

        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(
            PROGRESS_REFRESH_HZ,
        ));

        Self {
            mode: UiGroupMode::Progress { animated, multi },
        }
    }

    pub fn image_ui(&self, plain_prefix: impl Into<String>) -> Ui {
        match &self.mode {
            UiGroupMode::Quiet => Ui {
                mode: UiMode::Quiet,
            },
            UiGroupMode::Plain => Ui {
                mode: UiMode::Plain {
                    prefix: Some(plain_prefix.into()),
                },
            },
            UiGroupMode::Progress { animated, multi } => {
                let image = multi.add(ProgressBar::new_spinner());
                image.set_style(compose_image_style());
                if *animated {
                    image.enable_steady_tick(Duration::from_millis(100));
                }
                Ui {
                    mode: UiMode::Progress(Arc::new(ProgressUiInner {
                        animated: *animated,
                        aggregate_layers: true,
                        multi: multi.clone(),
                        image,
                        layers: Mutex::new(HashMap::new()),
                        layer_renders: Mutex::new(HashMap::new()),
                        aggregate: Mutex::new(AggregateProgress::default()),
                        image_name: Mutex::new(String::new()),
                    })),
                }
            }
        }
    }
}

impl ProgressUiInner {
    fn layer_bar(&self, digest: &str) -> Option<ProgressBar> {
        self.layers
            .lock()
            .expect("ui state poisoned")
            .get(digest)
            .cloned()
    }

    fn reset_for_image(&self) {
        self.image.reset();
        self.image.set_style(image_status_style(self.animated));
        if self.animated {
            self.image.enable_steady_tick(Duration::from_millis(100));
        }

        self.clear_layers();
    }

    fn clear_layers(&self) {
        let mut layers = self.layers.lock().expect("ui state poisoned");
        for (_, bar) in layers.drain() {
            bar.finish_and_clear();
            self.multi.remove(&bar);
        }
        self.layer_renders
            .lock()
            .expect("ui state poisoned")
            .clear();
        self.aggregate.lock().expect("ui state poisoned").clear();
    }

    fn start_layer_render(&self, digest: &str, total: u64, position: u64) {
        self.layer_renders
            .lock()
            .expect("ui state poisoned")
            .insert(
                digest.to_string(),
                LayerRender {
                    total,
                    position: position.min(total),
                    last_rendered_at: Some(Instant::now()),
                },
            );
    }

    fn advance_layer_render(&self, digest: &str, amount: u64) -> LayerRenderAdvance {
        let mut layer_renders = self.layer_renders.lock().expect("ui state poisoned");
        let Some(render) = layer_renders.get_mut(digest) else {
            return LayerRenderAdvance::Missing;
        };
        render.position = render.position.saturating_add(amount).min(render.total);
        if render
            .last_rendered_at
            .is_some_and(|last| last.elapsed() < DETAIL_RENDER_INTERVAL)
        {
            return LayerRenderAdvance::Throttled;
        }
        render.last_rendered_at = Some(Instant::now());
        LayerRenderAdvance::Render(render.position)
    }

    fn flush_layer_render(&self, digest: &str) -> Option<u64> {
        let mut layer_renders = self.layer_renders.lock().expect("ui state poisoned");
        let render = layer_renders.get_mut(digest)?;
        render.last_rendered_at = Some(Instant::now());
        Some(render.position)
    }

    fn finish_layer_render(&self, digest: &str) -> Option<u64> {
        let mut layer_renders = self.layer_renders.lock().expect("ui state poisoned");
        let mut render = layer_renders.remove(digest)?;
        if render.total > 0 {
            render.position = render.total;
        }
        Some(render.position)
    }

    fn prepare_aggregate_layers(&self, digests: &[String]) {
        let mut aggregate = self.aggregate.lock().expect("ui state poisoned");
        aggregate.clear();
        for digest in digests {
            aggregate.order.push(digest.clone());
            aggregate
                .layers
                .insert(digest.clone(), AggregateLayer::default());
        }
        drop(aggregate);
        self.render_aggregate_progress("Pulling");
    }

    fn start_aggregate_layer(&self, digest: &str, total_bytes: u64, starting_offset: u64) {
        let mut aggregate = self.aggregate.lock().expect("ui state poisoned");
        let layer = aggregate.touch_layer(digest);
        layer.total = total_bytes;
        layer.position = starting_offset.min(total_bytes);
        layer.state = if layer.position >= total_bytes && total_bytes > 0 {
            AggregateLayerState::Complete
        } else {
            AggregateLayerState::Downloading
        };
        layer.status = Some("Pulling".to_string());
        drop(aggregate);
        self.render_aggregate_progress("Pulling");
    }

    fn advance_aggregate_layer(&self, digest: &str, amount: u64) {
        let mut aggregate = self.aggregate.lock().expect("ui state poisoned");
        let layer = aggregate.touch_layer(digest);
        layer.position = layer.position.saturating_add(amount).min(layer.total);
        layer.state = if layer.total > 0 && layer.position >= layer.total {
            AggregateLayerState::Complete
        } else {
            AggregateLayerState::Downloading
        };
        layer.status = Some("Pulling".to_string());
        drop(aggregate);
        self.render_aggregate_progress_throttled("Pulling");
    }

    fn set_aggregate_layer_status(&self, digest: &str, status: &str) {
        let mut aggregate = self.aggregate.lock().expect("ui state poisoned");
        let layer = aggregate.touch_layer(digest);
        layer.status = Some(status.to_string());
        if layer.state != AggregateLayerState::Complete {
            layer.state = aggregate_layer_state_for_status(status);
        }
        drop(aggregate);
        self.render_aggregate_progress("Pulling");
    }

    fn finish_aggregate_layer(&self, digest: &str, status: &str) {
        let mut aggregate = self.aggregate.lock().expect("ui state poisoned");
        let layer = aggregate.touch_layer(digest);
        if layer.total > 0 {
            layer.position = layer.total;
        }
        layer.state = AggregateLayerState::Complete;
        layer.status = Some(status.to_string());
        drop(aggregate);
        self.render_aggregate_progress("Pulling");
    }

    fn render_aggregate_progress(&self, status: &str) {
        self.render_aggregate_progress_with_throttle(status, true);
    }

    fn render_aggregate_progress_throttled(&self, status: &str) {
        self.render_aggregate_progress_with_throttle(status, false);
    }

    fn render_aggregate_progress_with_throttle(&self, status: &str, force: bool) {
        let mut aggregate = self.aggregate.lock().expect("ui state poisoned");
        let render = aggregate_render(&aggregate, status);

        if aggregate.last_rendered.as_ref() == Some(&render) {
            return;
        }

        if !force
            && aggregate
                .last_rendered_at
                .is_some_and(|last| last.elapsed() < AGGREGATE_RENDER_INTERVAL)
        {
            return;
        }
        aggregate.last_rendered = Some(render.clone());
        aggregate.last_rendered_at = Some(Instant::now());
        drop(aggregate);

        self.image.set_style(compose_image_style());
        let image = self.image_name.lock().expect("ui state poisoned").clone();
        let bytes = if render.total_bytes > 0 && !render.hide_bytes {
            format!(
                " {} / {}",
                format_bytes(render.position.min(render.total_bytes)),
                format_bytes(render.total_bytes)
            )
        } else {
            String::new()
        };
        let layers = format!(
            " {}/{} layers",
            render.completed_layers, render.total_layers
        );
        self.image.set_message(format!(
            "{image} [{}]{layers}{bytes} {}",
            paint(&render.strip, GREEN),
            render.status
        ));
    }
}

fn aggregate_render(aggregate: &AggregateProgress, status: &str) -> AggregateRender {
    let mut strip = String::new();
    let mut total_bytes = 0;
    let mut position = 0;
    let mut hide_bytes = false;
    let mut completed_layers = 0;
    for digest in &aggregate.order {
        let Some(layer) = aggregate.layers.get(digest) else {
            continue;
        };
        total_bytes += layer.total;
        position += layer.position;
        if layer.state != AggregateLayerState::Complete && layer.total == 0 {
            hide_bytes = true;
        }
        if layer.state == AggregateLayerState::Complete {
            completed_layers += 1;
        }
        strip.push(layer_progress_char(layer));
    }

    AggregateRender {
        strip,
        total_bytes,
        position,
        hide_bytes,
        completed_layers,
        total_layers: aggregate.order.len(),
        status: status.to_string(),
    }
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

fn image_status_style(animated: bool) -> ProgressStyle {
    if animated {
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .expect("valid spinner template")
            .tick_strings(&["-", "\\", "|", "/"])
    } else {
        ProgressStyle::with_template("{msg}").expect("valid status template")
    }
}

fn layer_status_style(animated: bool) -> ProgressStyle {
    if animated {
        ProgressStyle::with_template(" {spinner:.cyan} {msg}")
            .expect("valid layer status template")
            .tick_strings(&[" ", ".", "o", "O", "o", "."])
    } else {
        ProgressStyle::with_template(" {msg}").expect("valid layer status template")
    }
}

fn layer_download_style() -> ProgressStyle {
    ProgressStyle::with_template(
        " {msg} [{bar:24.cyan/blue}] {bytes}/{total_bytes} {binary_bytes_per_sec}",
    )
    .expect("valid layer download template")
    .progress_chars("=> ")
}

fn compose_image_style() -> ProgressStyle {
    ProgressStyle::with_template("{msg}").expect("valid compose image template")
}

fn layer_progress_char(layer: &AggregateLayer) -> char {
    const PERCENT_CHARS: [char; 9] = ['⠀', '⡀', '⣀', '⣄', '⣤', '⣦', '⣶', '⣷', '⣿'];
    if layer.state == AggregateLayerState::Complete {
        return PERCENT_CHARS[PERCENT_CHARS.len() - 1];
    }
    if layer.total == 0 {
        return PERCENT_CHARS[0];
    }
    let percent = layer.position.saturating_mul(100) / layer.total;
    let index = (PERCENT_CHARS.len() as u64 - 1) * percent.min(100) / 100;
    PERCENT_CHARS[index as usize]
}

fn aggregate_layer_state_for_status(status: &str) -> AggregateLayerState {
    if status.contains("Pull complete") || status.contains("Already exists") {
        AggregateLayerState::Complete
    } else if status.contains("Waiting") {
        AggregateLayerState::Waiting
    } else {
        AggregateLayerState::Downloading
    }
}

pub(crate) fn should_color_stderr() -> bool {
    static SHOULD_COLOR_STDERR: OnceLock<bool> = OnceLock::new();

    *SHOULD_COLOR_STDERR
        .get_or_init(|| std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[cfg(target_os = "linux")]
    use super::linux_process_is_foreground_tty_job_from_stat;
    use super::{
        AggregateLayer, AggregateLayerState, AggregateProgress, LayerRenderAdvance,
        ProgressUiInner, Ui, UiMode, aggregate_render, compose_image_style,
        plain_layer_download_message,
    };
    use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget};

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
    fn aggregate_prepare_keeps_detailed_layer_bars() {
        let ui = aggregate_test_ui();
        let digests = vec![
            "sha256:first".to_string(),
            "sha256:second".to_string(),
            "sha256:third".to_string(),
        ];

        ui.prepare_layers(&digests);

        let inner = ui.progress().expect("test ui should be progress");
        let aggregate = inner.aggregate.lock().expect("ui state poisoned");
        assert_eq!(aggregate.order, digests);
        assert_eq!(aggregate.layers.len(), 3);
        drop(aggregate);
        assert_eq!(inner.layers.lock().expect("ui state poisoned").len(), 3);
    }

    #[test]
    fn aggregate_render_includes_layer_counts() {
        let mut aggregate = AggregateProgress {
            order: vec![
                "sha256:first".to_string(),
                "sha256:second".to_string(),
                "sha256:third".to_string(),
            ],
            layers: HashMap::new(),
            last_rendered: None,
            last_rendered_at: None,
        };
        aggregate.layers.insert(
            "sha256:first".to_string(),
            AggregateLayer {
                total: 100,
                position: 100,
                state: AggregateLayerState::Complete,
                status: Some("Pull complete".to_string()),
            },
        );
        aggregate.layers.insert(
            "sha256:second".to_string(),
            AggregateLayer {
                total: 100,
                position: 50,
                state: AggregateLayerState::Downloading,
                status: Some("Pulling".to_string()),
            },
        );
        aggregate
            .layers
            .insert("sha256:third".to_string(), AggregateLayer::default());

        let render = aggregate_render(&aggregate, "Pulling");

        assert_eq!(render.completed_layers, 1);
        assert_eq!(render.total_layers, 3);
        assert_eq!(render.status, "Pulling");
    }

    #[test]
    fn aggregate_render_throttles_fast_updates() {
        let ui = aggregate_test_ui();
        let inner = ui.progress().expect("test ui should be progress");
        inner.prepare_aggregate_layers(&["sha256:first".to_string()]);
        inner.start_aggregate_layer("sha256:first", 100, 0);
        let first_rendered_at = inner
            .aggregate
            .lock()
            .expect("ui state poisoned")
            .last_rendered_at
            .expect("forced render should be recorded");

        inner.advance_aggregate_layer("sha256:first", 10);

        let second_rendered_at = inner
            .aggregate
            .lock()
            .expect("ui state poisoned")
            .last_rendered_at
            .expect("render timestamp should remain present");
        assert_eq!(first_rendered_at, second_rendered_at);
    }

    #[test]
    fn aggregate_render_skips_forced_unchanged_summary() {
        let ui = aggregate_test_ui();
        let inner = ui.progress().expect("test ui should be progress");
        inner.prepare_aggregate_layers(&["sha256:first".to_string()]);
        let first_rendered_at = inner
            .aggregate
            .lock()
            .expect("ui state poisoned")
            .last_rendered_at
            .expect("forced render should be recorded");

        inner.set_aggregate_layer_status("sha256:first", "Waiting for another pocker process");

        let second_rendered_at = inner
            .aggregate
            .lock()
            .expect("ui state poisoned")
            .last_rendered_at
            .expect("render timestamp should remain present");
        assert_eq!(first_rendered_at, second_rendered_at);
    }

    #[test]
    fn detailed_layer_render_throttles_fast_updates() {
        let ui = aggregate_test_ui();
        let inner = ui.progress().expect("test ui should be progress");
        inner.start_layer_render("sha256:first", 100, 0);

        assert_eq!(
            inner.advance_layer_render("sha256:first", 10),
            LayerRenderAdvance::Throttled
        );

        let render = inner
            .layer_renders
            .lock()
            .expect("ui state poisoned")
            .get("sha256:first")
            .map(|render| render.position);
        assert_eq!(render, Some(10));
        assert_eq!(inner.flush_layer_render("sha256:first"), Some(10));
        assert_eq!(inner.finish_layer_render("sha256:first"), Some(100));
    }

    fn aggregate_test_ui() -> Ui {
        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
        let image = multi.add(ProgressBar::new_spinner());
        image.set_style(compose_image_style());
        Ui {
            mode: UiMode::Progress(Arc::new(ProgressUiInner {
                animated: false,
                aggregate_layers: true,
                multi,
                image,
                layers: Mutex::new(HashMap::new()),
                layer_renders: Mutex::new(HashMap::new()),
                aggregate: Mutex::new(AggregateProgress::default()),
                image_name: Mutex::new("example:latest".to_string()),
            })),
        }
    }
}
