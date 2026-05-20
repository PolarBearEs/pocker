use std::time::{SystemTime, UNIX_EPOCH};

use crate::docker;
use crate::error::{DockerPullError, Result};

pub(crate) async fn print_image_list() -> Result<()> {
    let images = docker::list_images().await?;
    let mut rows = Vec::new();
    for image in images {
        if image.repo_tags.is_empty() {
            rows.push((
                "<none>".to_string(),
                "<none>".to_string(),
                short_image_id(&image.id).to_string(),
                format_created(image.created),
                format_size(image.size),
            ));
            continue;
        }

        for tag in image.repo_tags {
            let (repository, tag) = split_repo_tag(&tag);
            rows.push((
                repository.to_string(),
                tag.to_string(),
                short_image_id(&image.id).to_string(),
                format_created(image.created),
                format_size(image.size),
            ));
        }
    }

    let repo_width = rows
        .iter()
        .map(|(repository, _, _, _, _)| repository.len())
        .max()
        .unwrap_or(0)
        .max("REPOSITORY".len());
    let tag_width = rows
        .iter()
        .map(|(_, tag, _, _, _)| tag.len())
        .max()
        .unwrap_or(0)
        .max("TAG".len());
    let id_width = rows
        .iter()
        .map(|(_, _, image_id, _, _)| image_id.len())
        .max()
        .unwrap_or(0)
        .max("IMAGE ID".len());
    let created_width = rows
        .iter()
        .map(|(_, _, _, created, _)| created.len())
        .max()
        .unwrap_or(0)
        .max("CREATED".len());

    println!(
        "{:<repo_width$}  {:<tag_width$}  {:<id_width$}  {:<created_width$}  SIZE",
        "REPOSITORY", "TAG", "IMAGE ID", "CREATED",
    );
    for (repository, tag, image_id, created, size) in rows {
        println!(
            "{:<repo_width$}  {:<tag_width$}  {:<id_width$}  {:<created_width$}  {}",
            repository, tag, image_id, created, size,
        );
    }
    Ok(())
}

pub(crate) async fn print_image_inspect(reference: &str) -> Result<()> {
    let Some(image) = docker::inspect_image(reference).await? else {
        return Err(DockerPullError::CommandFailed(format!(
            "docker image inspect failed: image `{reference}` not found"
        )));
    };
    println!("{}", serde_json::to_string_pretty(&image)?);
    Ok(())
}

fn split_repo_tag(value: &str) -> (&str, &str) {
    match value.rsplit_once(':') {
        Some((repository, tag)) if !tag.contains('/') => (repository, tag),
        _ => (value, "<none>"),
    }
}

fn short_image_id(value: &str) -> &str {
    let value = value.strip_prefix("sha256:").unwrap_or(value);
    let end = value
        .char_indices()
        .nth(12)
        .map(|(index, _)| index)
        .unwrap_or(value.len());
    &value[..end]
}

pub(crate) fn format_size(size: Option<u64>) -> String {
    let Some(size) = size else {
        return "<unknown>".into();
    };
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = size as f64;
    let mut unit = 0usize;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{size} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_created(created: Option<i64>) -> String {
    let Some(created) = created else {
        return "<unknown>".into();
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return "<unknown>".into();
    };
    let now = now.as_secs();
    let created = if created < 0 { 0 } else { created as u64 };
    if created > now {
        return "just now".into();
    }

    let delta = now - created;
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const MONTH: u64 = 30 * DAY;
    const YEAR: u64 = 365 * DAY;

    let (value, unit) = if delta < MINUTE {
        (delta, "second")
    } else if delta < HOUR {
        (delta / MINUTE, "minute")
    } else if delta < DAY {
        (delta / HOUR, "hour")
    } else if delta < WEEK {
        (delta / DAY, "day")
    } else if delta < MONTH {
        (delta / WEEK, "week")
    } else if delta < YEAR {
        (delta / MONTH, "month")
    } else {
        (delta / YEAR, "year")
    };

    if value == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{value} {unit}s ago")
    }
}
