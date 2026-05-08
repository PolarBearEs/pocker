use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs;
use std::io::IsTerminal;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

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
}

#[derive(Default)]
struct AggregateLayer {
    total: u64,
    position: u64,
    complete: bool,
}

impl AggregateProgress {
    fn clear(&mut self) {
        self.order.clear();
        self.layers.clear();
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
        if let Some(inner) = self.progress() {
            inner.reset_for_image();
            *inner.image_name.lock().expect("ui state poisoned") = image.to_string();
            if inner.aggregate_layers {
                inner.image.set_style(compose_image_style());
            }
            inner.image.set_message(format!("{image} Pulling"));
        } else if self.is_plain() {
            self.plain_line(format!("image {image}: Pulling"));
        }
    }

    pub fn begin_load(&self, image: &str) {
        if let Some(inner) = self.progress() {
            if inner.aggregate_layers {
                inner.image.set_style(image_status_style(false));
            }
            inner.image.set_message(format!("{image} Loading"));
        } else if self.is_plain() {
            self.plain_line(format!("image {image}: Loading"));
        }
    }

    pub fn set_image_status(&self, image: &str, status: &str) {
        if let Some(inner) = self.progress() {
            if inner.aggregate_layers {
                inner.image.set_style(image_status_style(false));
            }
            inner.image.set_message(format!("{image} {status}"));
        } else if self.is_plain() {
            self.plain_line(format!("image {image}: {status}"));
        }
    }

    pub fn finish_image(&self, image: &str, status: &str) {
        if let Some(inner) = self.progress() {
            inner.clear_layers();
            inner.image.disable_steady_tick();
            if inner.aggregate_layers {
                inner.image.set_style(image_status_style(false));
            }
            inner.image.finish_with_message(format!("{image} {status}"));
        } else if self.is_plain() {
            self.plain_line(format!("image {image}: {status}"));
        }
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
        if let Some(inner) = self.progress()
            && inner.aggregate_layers
        {
            inner.start_aggregate_layer(digest, total_bytes, starting_offset);
        }
        let Some(bar) = self.layer_bar(digest) else {
            if self.is_plain() {
                self.plain_line(plain_layer_download_message(
                    digest,
                    total_bytes,
                    starting_offset,
                ));
            }
            return;
        };
        bar.set_style(layer_download_style());
        bar.set_length(total_bytes);
        bar.set_position(starting_offset);
        bar.set_message(short_digest(digest));
    }

    pub fn advance_layer_download(&self, digest: &str, amount: u64) {
        if let Some(inner) = self.progress()
            && inner.aggregate_layers
        {
            inner.advance_aggregate_layer(digest, amount);
        }
        let Some(bar) = self.layer_bar(digest) else {
            return;
        };
        bar.inc(amount);
    }

    pub fn finish_layer_download(&self, digest: &str) {
        self.finish_layer_status(digest, "Pull complete", "Pull complete");
    }

    pub fn set_layer_status(&self, digest: &str, status: &str) {
        let Some(bar) = self.layer_bar(digest) else {
            if self.is_plain() {
                self.plain_line(format!("layer {}: {status}", short_digest(digest)));
            }
            return;
        };
        bar.set_style(layer_status_style(self.is_animated()));
        if self.is_animated() {
            bar.enable_steady_tick(Duration::from_millis(120));
        }
        bar.set_message(format!("{} {status}", short_digest(digest)));
    }

    pub fn warn(&self, message: impl Into<String>) {
        let message = format!("warning: {}", message.into());
        if let Some(inner) = self.progress() {
            inner.image.println(message);
        } else if self.is_plain() {
            self.plain_line(message);
        }
    }

    fn finish_layer_status(&self, digest: &str, progress_status: &str, plain_status: &str) {
        if let Some(inner) = self.progress()
            && inner.aggregate_layers
        {
            inner.finish_aggregate_layer(digest);
        }
        let Some(bar) = self.layer_bar(digest) else {
            if self.is_plain() {
                self.plain_line(format!("layer {}: {plain_status}", short_digest(digest)));
            }
            return;
        };
        bar.set_style(layer_status_style(self.is_animated()));
        bar.disable_steady_tick();
        bar.finish_with_message(format!("{} {progress_status}", short_digest(digest)));
    }

    fn layer_bar(&self, digest: &str) -> Option<ProgressBar> {
        let UiMode::Progress(inner) = &self.mode else {
            return None;
        };
        inner
            .layers
            .lock()
            .expect("ui state poisoned")
            .get(digest)
            .cloned()
    }

    fn progress(&self) -> Option<&Arc<ProgressUiInner>> {
        match &self.mode {
            UiMode::Progress(inner) => Some(inner),
            UiMode::Quiet | UiMode::Plain { .. } => None,
        }
    }

    fn is_animated(&self) -> bool {
        self.progress().map(|inner| inner.animated).unwrap_or(false)
    }

    fn is_plain(&self) -> bool {
        matches!(self.mode, UiMode::Plain { .. })
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
        if !aggregate.layers.contains_key(digest) {
            aggregate.order.push(digest.to_string());
        }
        let layer = aggregate.layers.entry(digest.to_string()).or_default();
        layer.total = total_bytes;
        layer.position = starting_offset.min(total_bytes);
        layer.complete = layer.position >= total_bytes && total_bytes > 0;
        drop(aggregate);
        self.render_aggregate_progress("Pulling");
    }

    fn advance_aggregate_layer(&self, digest: &str, amount: u64) {
        let mut aggregate = self.aggregate.lock().expect("ui state poisoned");
        if !aggregate.layers.contains_key(digest) {
            aggregate.order.push(digest.to_string());
        }
        let layer = aggregate.layers.entry(digest.to_string()).or_default();
        layer.position = layer.position.saturating_add(amount).min(layer.total);
        layer.complete = layer.total > 0 && layer.position >= layer.total;
        drop(aggregate);
        self.render_aggregate_progress("Pulling");
    }

    fn finish_aggregate_layer(&self, digest: &str) {
        let mut aggregate = self.aggregate.lock().expect("ui state poisoned");
        if !aggregate.layers.contains_key(digest) {
            aggregate.order.push(digest.to_string());
        }
        let layer = aggregate.layers.entry(digest.to_string()).or_default();
        if layer.total > 0 {
            layer.position = layer.total;
        }
        layer.complete = true;
        drop(aggregate);
        self.render_aggregate_progress("Pulling");
    }

    fn render_aggregate_progress(&self, status: &str) {
        let aggregate = self.aggregate.lock().expect("ui state poisoned");
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
        drop(aggregate);

        self.image.set_style(compose_image_style());
        let image = self.image_name.lock().expect("ui state poisoned").clone();
        let bytes = if total_bytes > 0 && !hide_bytes {
            format!(
                " {} / {}",
                format_bytes(position.min(total_bytes)),
                format_bytes(total_bytes)
            )
        } else {
            String::new()
        };
        self.image.set_message(format!(
            "{image} [{}]{bytes} {status}",
            success_color(&strip)
        ));
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
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
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

fn success_color(value: &str) -> String {
    if should_color_stderr() {
        format!("\x1b[32m{value}\x1b[0m")
    } else {
        value.to_string()
    }
}

pub(crate) fn should_color_stderr() -> bool {
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
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
