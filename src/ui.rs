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
enum UiMode {
    Quiet,
    Progress(Arc<ProgressUiInner>),
    Plain(Arc<PlainUiInner>),
}

struct ProgressUiInner {
    animated: bool,
    multi: MultiProgress,
    image: ProgressBar,
    layers: Mutex<HashMap<String, ProgressBar>>,
}

struct PlainUiInner {
    output: Mutex<()>,
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
                mode: UiMode::Plain(Arc::new(PlainUiInner {
                    output: Mutex::new(()),
                })),
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
                multi,
                image,
                layers: Mutex::new(HashMap::new()),
            })),
        }
    }

    pub fn begin_image(&self, image: &str) {
        if let Some(inner) = self.progress() {
            inner.image.set_message(format!("{image} Pulling"));
        } else if let Some(inner) = self.plain() {
            inner.println(format!("image {image}: Pulling"));
        }
    }

    pub fn begin_load(&self, image: &str) {
        if let Some(inner) = self.progress() {
            inner.image.set_message(format!("{image} Loading"));
        } else if let Some(inner) = self.plain() {
            inner.println(format!("image {image}: Loading"));
        }
    }

    pub fn set_image_status(&self, image: &str, status: &str) {
        if let Some(inner) = self.progress() {
            inner.image.set_message(format!("{image} {status}"));
        } else if let Some(inner) = self.plain() {
            inner.println(format!("image {image}: {status}"));
        }
    }

    pub fn finish_image(&self, image: &str, status: &str) {
        if let Some(inner) = self.progress() {
            inner.image.finish_with_message(format!("{image} {status}"));
        } else if let Some(inner) = self.plain() {
            inner.println(format!("image {image}: {status}"));
        }
    }

    pub fn prepare_layers(&self, digests: &[String]) {
        let Some(inner) = self.progress() else {
            return;
        };
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
        let Some(bar) = self.layer_bar(digest) else {
            if let Some(inner) = self.plain() {
                inner.println(plain_layer_download_message(
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
            if let Some(inner) = self.plain() {
                inner.println(format!("layer {}: {status}", short_digest(digest)));
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
        } else if let Some(inner) = self.plain() {
            inner.println(message);
        }
    }

    fn finish_layer_status(&self, digest: &str, progress_status: &str, plain_status: &str) {
        let Some(bar) = self.layer_bar(digest) else {
            if let Some(inner) = self.plain() {
                inner.println(format!("layer {}: {plain_status}", short_digest(digest)));
            }
            return;
        };
        bar.set_style(layer_status_style(self.is_animated()));
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
            UiMode::Quiet | UiMode::Plain(_) => None,
        }
    }

    fn plain(&self) -> Option<&Arc<PlainUiInner>> {
        match &self.mode {
            UiMode::Plain(inner) => Some(inner),
            UiMode::Quiet | UiMode::Progress(_) => None,
        }
    }

    fn is_animated(&self) -> bool {
        self.progress().map(|inner| inner.animated).unwrap_or(false)
    }
}

impl PlainUiInner {
    fn println(&self, message: impl AsRef<str>) {
        let _output = self.output.lock().expect("ui state poisoned");
        eprintln!("{}", message.as_ref());
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
