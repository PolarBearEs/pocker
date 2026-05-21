use std::path::Path;

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde_json::Value;
use tokio::io::DuplexStream;
use tokio_util::io::{ReaderStream, SyncIoBridge};

use crate::error::{DockerPullError, Result};
use crate::export::oci_archive::{
    PreparedOciArchive, prepare_oci_archive, write_prepared_oci_archive_to_writer,
};
use crate::reference::{ImageReference, ReferenceTarget};
use crate::store::{Store, StoredReference};

mod daemon;
pub(crate) mod layers;
mod transport;

use daemon::DockerDaemon;

pub(crate) use layers::{
    MaterializedDaemonLayers, daemon_layer_coverage, materialize_daemon_layers,
};

const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b':')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');
const QUERY_VALUE_ENCODE_SET: &AsciiSet = &PATH_SEGMENT_ENCODE_SET.add(b'&').add(b'=').add(b'+');
const LOAD_ARCHIVE_STREAM_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ImageSummary {
    pub created: Option<i64>,
    pub id: String,
    pub repo_tags: Vec<String>,
    pub size: Option<u64>,
}

pub async fn load_reference_archive_stream(
    store: &Store,
    reference: &StoredReference,
) -> Result<()> {
    let daemon = DockerDaemon::connect()?;
    let prepared = prepare_oci_archive(store, reference).await?;
    let (reader, writer) = tokio::io::duplex(LOAD_ARCHIVE_STREAM_BUFFER_BYTES);
    let writer = SyncIoBridge::new(writer);
    let store = store.clone();
    let reference = reference.clone();
    let writer = tokio::task::spawn_blocking(move || {
        write_archive_to_stream(writer, store, reference, prepared)
    });

    let load_result = daemon.load_archive_stream(ReaderStream::new(reader)).await;
    if let Err(error) = load_result {
        match writer.await {
            Ok(Err(writer_error)) => return Err(writer_error),
            Err(join_error) => {
                return Err(DockerPullError::CommandFailed(format!(
                    "archive stream writer failed: {join_error}"
                )));
            }
            Ok(Ok(())) => {}
        }
        return Err(error);
    }

    writer.await.map_err(|error| {
        DockerPullError::CommandFailed(format!("archive stream writer failed: {error}"))
    })?
}

fn write_archive_to_stream(
    mut writer: SyncIoBridge<DuplexStream>,
    store: Store,
    reference: StoredReference,
    prepared: PreparedOciArchive,
) -> Result<()> {
    write_prepared_oci_archive_to_writer(&mut writer, &store, &reference, &prepared)?;
    writer.shutdown()?;
    Ok(())
}

pub async fn daemon_has_reference(reference: &ImageReference, config_digest: &str) -> Result<bool> {
    let inspect_target = daemon_inspect_target(reference, config_digest);
    let daemon = DockerDaemon::connect()?;
    let Some(image) = daemon.inspect_image(&inspect_target).await? else {
        return Ok(false);
    };

    Ok(normalize_image_id(&image.id) == normalize_image_id(config_digest))
}

fn daemon_inspect_target(reference: &ImageReference, config_digest: &str) -> String {
    match reference.target {
        ReferenceTarget::Tag(_) => reference.display_name(),
        ReferenceTarget::Digest(_) => config_digest.to_string(),
    }
}

fn normalize_image_id(image_id: &str) -> &str {
    image_id.strip_prefix("sha256:").unwrap_or(image_id).trim()
}

pub async fn load_archive(path: &Path) -> Result<()> {
    DockerDaemon::connect()?.load_archive(path).await
}

pub async fn pull_image(reference: &str) -> Result<()> {
    DockerDaemon::connect()?.pull_image(reference).await
}

pub async fn tag_image(source: &str, target: &str) -> Result<()> {
    DockerDaemon::connect()?.tag_image(source, target).await
}

pub async fn remove_image_tag(reference: &str) -> Result<()> {
    DockerDaemon::connect()?.remove_image_tag(reference).await
}

pub async fn inspect_image(reference: &str) -> Result<Option<Value>> {
    DockerDaemon::connect()?
        .inspect_image_value(reference)
        .await
}

pub async fn list_images() -> Result<Vec<ImageSummary>> {
    let daemon = DockerDaemon::connect()?;
    let images = daemon.list_image_summaries().await?;
    Ok(images
        .into_iter()
        .map(|image| ImageSummary {
            created: image.created,
            id: image.id,
            repo_tags: image.repo_tags.unwrap_or_default(),
            size: image.size,
        })
        .collect())
}

pub async fn save_image(reference: &str, path: &Path) -> Result<()> {
    DockerDaemon::connect()?.save_image(reference, path).await
}

fn encode_path_segment(value: &str) -> String {
    utf8_percent_encode(value, PATH_SEGMENT_ENCODE_SET).to_string()
}

fn encode_query_value(value: &str) -> String {
    utf8_percent_encode(value, QUERY_VALUE_ENCODE_SET).to_string()
}

fn split_tagged_reference(reference: &str) -> Result<(&str, &str)> {
    if reference.contains('@') {
        return Err(DockerPullError::InvalidInput(format!(
            "image reference `{reference}` is not a tagged reference"
        )));
    }
    let slash = reference.rfind('/');
    let colon = reference.rfind(':').ok_or_else(|| {
        DockerPullError::InvalidInput(format!("image reference `{reference}` is missing a tag"))
    })?;
    if slash.is_some_and(|slash| colon < slash) {
        return Err(DockerPullError::InvalidInput(format!(
            "image reference `{reference}` is missing a tag"
        )));
    }
    Ok((&reference[..colon], &reference[colon + 1..]))
}

fn ensure_json_stream_success(body: String, action: &str) -> Result<()> {
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(error) = value.get("error").and_then(|value| value.as_str()) {
            return Err(DockerPullError::CommandFailed(format!(
                "{action} failed: {error}"
            )));
        }
        if let Some(error) = value
            .get("errorDetail")
            .and_then(|value| value.get("message"))
            .and_then(|value| value.as_str())
        {
            return Err(DockerPullError::CommandFailed(format!(
                "{action} failed: {error}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::path::Path;

    use super::transport::windows::{decode_chunked_body, header_value, parse_response_head};
    use super::transport::{DEFAULT_DOCKER_HOST, DockerEndpoint, docker_endpoint_from_host};
    use super::{daemon_inspect_target, encode_path_segment, encode_query_value};
    use super::{layers::ordered_unique_image_ids, split_tagged_reference};
    use crate::reference::ImageReference;

    #[test]
    fn inspect_target_uses_display_name_for_tagged_references() {
        let reference = ImageReference::parse("alpine:latest").expect("reference should parse");
        assert_eq!(
            daemon_inspect_target(&reference, "sha256:deadbeef"),
            "alpine:latest"
        );
    }

    #[test]
    fn inspect_target_uses_config_digest_for_digest_references() {
        let reference =
            ImageReference::parse("ghcr.io/acme/app@sha256:beef").expect("reference should parse");
        assert_eq!(
            daemon_inspect_target(&reference, "sha256:deadbeef"),
            "sha256:deadbeef"
        );
    }

    #[test]
    fn ordered_unique_image_ids_preserves_first_seen_order() {
        let ids = ordered_unique_image_ids(vec![
            super::daemon::DaemonImageSummary {
                id: "sha256:1".into(),
                created: None,
                repo_tags: None,
                size: None,
            },
            super::daemon::DaemonImageSummary {
                id: "sha256:2".into(),
                created: None,
                repo_tags: None,
                size: None,
            },
            super::daemon::DaemonImageSummary {
                id: "sha256:1".into(),
                created: None,
                repo_tags: None,
                size: None,
            },
        ]);

        assert_eq!(ids, vec!["sha256:1", "sha256:2"]);
    }

    #[test]
    fn split_tagged_reference_handles_registry_ports() {
        assert_eq!(
            split_tagged_reference("127.0.0.1:5000/pocker/image:latest")
                .expect("reference should split"),
            ("127.0.0.1:5000/pocker/image", "latest")
        );
    }

    #[test]
    fn split_tagged_reference_rejects_digest_references() {
        assert!(split_tagged_reference("alpine@sha256:deadbeef").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn docker_host_defaults_to_unix_socket() {
        let endpoint = docker_endpoint_from_host(DEFAULT_DOCKER_HOST)
            .expect("default docker host should parse");
        assert!(
            matches!(endpoint, DockerEndpoint::Unix(path) if path == Path::new("/var/run/docker.sock"))
        );
    }

    #[test]
    fn docker_host_supports_tcp() {
        let endpoint = docker_endpoint_from_host("tcp://127.0.0.1:2375")
            .expect("tcp docker host should parse");
        assert!(matches!(endpoint, DockerEndpoint::Http(base) if base == "http://127.0.0.1:2375"));
    }

    #[cfg(not(windows))]
    #[test]
    fn docker_host_named_pipe_reports_actionable_error_on_all_hosts() {
        let error = docker_endpoint_from_host("npipe:////./pipe/docker_engine")
            .expect_err("named pipe host should be rejected explicitly");
        assert_eq!(
            error.to_string(),
            "docker named pipes are not supported; set DOCKER_HOST to a tcp://, http://, or https:// endpoint"
        );
    }

    #[cfg(windows)]
    #[test]
    fn docker_host_supports_windows_named_pipe() {
        let endpoint = docker_endpoint_from_host(DEFAULT_DOCKER_HOST)
            .expect("windows named pipe host should parse");
        assert!(
            matches!(endpoint, DockerEndpoint::NamedPipe(path) if path == std::path::Path::new(r"\\.\pipe\docker_engine"))
        );
    }

    #[test]
    fn docker_host_trims_trailing_slashes() {
        let endpoint = docker_endpoint_from_host("https://docker.example.test///")
            .expect("https docker host should parse");
        assert!(
            matches!(endpoint, DockerEndpoint::Http(base) if base == "https://docker.example.test")
        );
    }

    #[test]
    fn docker_api_response_head_parses_status_and_headers() {
        let (status, headers) =
            parse_response_head(b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\n\r\n")
                .expect("response head should parse");

        assert_eq!(status, reqwest::StatusCode::CREATED);
        assert_eq!(
            header_value(&headers, "content-type"),
            Some("application/json")
        );
    }

    #[test]
    fn docker_api_chunked_body_decodes() {
        let body = decode_chunked_body(b"4\r\npock\r\n2\r\ner\r\n0\r\n\r\n")
            .expect("chunked body should decode");

        assert_eq!(body, b"pocker");
    }

    #[test]
    fn encodes_image_names_for_api_paths() {
        assert_eq!(
            encode_path_segment("docker.io/library/alpine:latest"),
            "docker.io%2Flibrary%2Falpine%3Alatest"
        );
    }

    #[test]
    fn encodes_image_names_for_api_query_values() {
        assert_eq!(
            encode_query_value("example.com/acme/app&name=value+tag:latest"),
            "example.com%2Facme%2Fapp%26name%3Dvalue%2Btag%3Alatest"
        );
    }
}
