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

const AGGREGATE_RENDER_INTERVAL: Duration = Duration::from_millis(50);

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
    aggregate: Mutex<AggregateProgress>,
    image_name: Mutex<String>,
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
    complete: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct AggregateRender {
    strip: String,
    total_bytes: u64,
    position: u64,
    hide_bytes: bool,
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

        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr());
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
                if let Some(bar) = inner.layer_bar(digest) {
                    bar.inc(amount);
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
                let Some(bar) = inner.layer_bar(digest) else {
                    return;
                };
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

    pub fn warn(&self, message: impl Into<String>) {
        let message = format!("warning: {}", message.into());
        let plain_message = message.clone();
        self.dispatch_mode(
            |inner| inner.image.println(message),
            || self.plain_line(plain_message),
        );
    }

    fn finish_layer_status(&self, digest: &str, progress_status: &str, plain_status: &str) {
        self.dispatch_mode(
            |inner| {
                if inner.aggregate_layers {
                    inner.finish_aggregate_layer(digest);
                }
                let Some(bar) = inner.layer_bar(digest) else {
                    return;
                };
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
                animated,
                multi: MultiProgress::with_draw_target(ProgressDrawTarget::stderr()),
            },
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
        self.aggregate.lock().expect("ui state poisoned").clear();
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
        layer.complete = layer.position >= total_bytes && total_bytes > 0;
        drop(aggregate);
        self.render_aggregate_progress("Pulling");
    }

    fn advance_aggregate_layer(&self, digest: &str, amount: u64) {
        let mut aggregate = self.aggregate.lock().expect("ui state poisoned");
        let layer = aggregate.touch_layer(digest);
        layer.position = layer.position.saturating_add(amount).min(layer.total);
        layer.complete = layer.total > 0 && layer.position >= layer.total;
        drop(aggregate);
        self.render_aggregate_progress_throttled("Pulling");
    }

    fn finish_aggregate_layer(&self, digest: &str) {
        let mut aggregate = self.aggregate.lock().expect("ui state poisoned");
        let layer = aggregate.touch_layer(digest);
        if layer.total > 0 {
            layer.position = layer.total;
        }
        layer.complete = true;
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

        if !force {
            if aggregate.last_rendered.as_ref() == Some(&render) {
                return;
            }
            if aggregate
                .last_rendered_at
                .is_some_and(|last| last.elapsed() < AGGREGATE_RENDER_INTERVAL)
            {
                return;
            }
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
        self.image.set_message(format!(
            "{image} [{}]{bytes} {}",
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
    for digest in &aggregate.order {
        let Some(layer) = aggregate.layers.get(digest) else {
            continue;
        };
        total_bytes += layer.total;
        position += layer.position;
        if !layer.complete && layer.total == 0 {
            hide_bytes = true;
        }
        strip.push(layer_progress_char(layer));
    }

    AggregateRender {
        strip,
        total_bytes,
        position,
        hide_bytes,
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
    if layer.complete {
        return PERCENT_CHARS[PERCENT_CHARS.len() - 1];
    }
    if layer.total == 0 {
        return PERCENT_CHARS[0];
    }
    let percent = layer.position.saturating_mul(100) / layer.total;
    let index = (PERCENT_CHARS.len() as u64 - 1) * percent.min(100) / 100;
    PERCENT_CHARS[index as usize]
}

pub(crate) fn should_color_stderr() -> bool {
    static SHOULD_COLOR_STDERR: OnceLock<bool> = OnceLock::new();

    *SHOULD_COLOR_STDERR
        .get_or_init(|| std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

#[cfg(test)]
mod tests {
    use super::plain_layer_download_message;

    #[cfg(target_os = "linux")]
    use super::linux_process_is_foreground_tty_job_from_stat;

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
}
