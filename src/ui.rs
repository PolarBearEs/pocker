use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs;
use std::io::IsTerminal;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

#[derive(Clone)]
pub struct Ui {
    inner: Option<Arc<UiInner>>,
}

struct UiInner {
    multi: MultiProgress,
    image: ProgressBar,
    layers: Mutex<HashMap<String, ProgressBar>>,
}

impl Ui {
    pub fn new(quiet: bool) -> Self {
        if quiet || !should_render_progress() {
            return Self { inner: None };
        }

        let multi = MultiProgress::with_draw_target(ProgressDrawTarget::stderr());
        let image = multi.add(ProgressBar::new_spinner());
        image.set_style(spinner_style());
        image.enable_steady_tick(Duration::from_millis(100));

        Self {
            inner: Some(Arc::new(UiInner {
                multi,
                image,
                layers: Mutex::new(HashMap::new()),
            })),
        }
    }

    pub fn begin_image(&self, image: &str) {
        let Some(inner) = &self.inner else {
            return;
        };
        inner.image.set_message(format!("{image} Pulling"));
    }

    pub fn begin_load(&self, image: &str) {
        let Some(inner) = &self.inner else {
            return;
        };
        inner.image.set_message(format!("{image} Loading"));
    }

    pub fn set_image_status(&self, image: &str, status: &str) {
        let Some(inner) = &self.inner else {
            return;
        };
        inner.image.set_message(format!("{image} {status}"));
    }

    pub fn finish_image(&self, image: &str, status: &str) {
        let Some(inner) = &self.inner else {
            return;
        };
        inner.image.finish_with_message(format!("{image} {status}"));
    }

    pub fn prepare_layers(&self, digests: &[String]) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut layers = inner.layers.lock().expect("ui state poisoned");
        for digest in digests {
            let bar = inner.multi.add(ProgressBar::new_spinner());
            bar.set_style(layer_status_style());
            bar.enable_steady_tick(Duration::from_millis(120));
            bar.set_message(format!("{} Pulling fs layer", short_digest(digest)));
            layers.insert(digest.clone(), bar);
        }
    }

    pub fn mark_layer_cached(&self, digest: &str) {
        self.finish_layer_status(digest, "Already exists");
    }

    pub fn mark_layer_daemon(&self, digest: &str) {
        self.finish_layer_status(digest, "Already exists");
    }

    pub fn start_layer_download(&self, digest: &str, total_bytes: u64, starting_offset: u64) {
        let Some(bar) = self.layer_bar(digest) else {
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
        self.finish_layer_status(digest, "Pull complete");
    }

    pub fn set_layer_status(&self, digest: &str, status: &str) {
        let Some(bar) = self.layer_bar(digest) else {
            return;
        };
        bar.set_style(layer_status_style());
        bar.enable_steady_tick(Duration::from_millis(120));
        bar.set_message(format!("{} {status}", short_digest(digest)));
    }

    pub fn warn(&self, message: impl Into<String>) {
        let Some(inner) = &self.inner else {
            return;
        };
        inner.image.println(format!("warning: {}", message.into()));
    }

    fn finish_layer_status(&self, digest: &str, status: &str) {
        let Some(bar) = self.layer_bar(digest) else {
            return;
        };
        bar.set_style(layer_status_style());
        bar.finish_with_message(format!("{} {status}", short_digest(digest)));
    }

    fn layer_bar(&self, digest: &str) -> Option<ProgressBar> {
        let inner = self.inner.as_ref()?;
        inner
            .layers
            .lock()
            .expect("ui state poisoned")
            .get(digest)
            .cloned()
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

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} {msg}")
        .expect("valid spinner template")
        .tick_strings(&["-", "\\", "|", "/"])
}

fn layer_status_style() -> ProgressStyle {
    ProgressStyle::with_template(" {spinner:.cyan} {msg}")
        .expect("valid layer status template")
        .tick_strings(&[" ", ".", "o", "O", "o", "."])
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
}
