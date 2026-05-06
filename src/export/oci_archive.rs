use std::fs::File;
use std::io::Write;
use std::path::Path;

use serde::Serialize;
use tar::{Builder, Header};

use crate::docker;
use crate::error::{DockerPullError, Result};
use crate::image::parse_diff_ids;
use crate::reference::{ImageReference, ReferenceTarget};
use crate::registry::Descriptor;
use crate::store::Store;
use crate::store::StoredReference;

#[derive(Debug, Serialize)]
struct OciLayout<'a> {
    #[serde(rename = "imageLayoutVersion")]
    image_layout_version: &'a str,
}

#[derive(Debug, Serialize)]
struct IndexJson {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    manifests: Vec<Descriptor>,
}

#[derive(Debug, Serialize)]
struct DockerManifestEntry {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "RepoTags")]
    repo_tags: Vec<String>,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

pub async fn write_oci_archive(
    path: &Path,
    store: &Store,
    reference: &StoredReference,
) -> Result<()> {
    let file = File::create(path)?;
    write_oci_archive_to_writer(file, store, reference).await
}

pub async fn write_oci_archive_to_writer<W: Write>(
    writer: W,
    store: &Store,
    reference: &StoredReference,
) -> Result<()> {
    let mut builder = Builder::new(writer);
    append_json(
        &mut builder,
        "oci-layout",
        &OciLayout {
            image_layout_version: "1.0.0",
        },
    )?;

    let mut manifest_descriptor = reference.manifest.clone();
    if let Some(ref_name) = oci_ref_name(&reference.reference)? {
        manifest_descriptor.annotations = Some(
            [("org.opencontainers.image.ref.name".to_string(), ref_name)]
                .into_iter()
                .collect(),
        );
    }
    append_json(
        &mut builder,
        "index.json",
        &IndexJson {
            schema_version: 2,
            manifests: vec![manifest_descriptor.clone()],
        },
    )?;

    append_blob(&mut builder, store, &manifest_descriptor.digest)?;
    append_blob(&mut builder, store, &reference.config_digest)?;

    let manifest_path = store.blob_path(&manifest_descriptor.digest)?;
    let manifest_bytes = std::fs::read(manifest_path)?;
    let config_path = store.blob_path(&reference.config_digest)?;
    let config_bytes = std::fs::read(config_path)?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
    let diff_ids = parse_diff_ids(&config_bytes)?;
    let layers = manifest
        .get("layers")
        .and_then(|layers| layers.as_array())
        .ok_or_else(|| DockerPullError::BadResponse("manifest layers missing".into()))?;
    if diff_ids.len() != layers.len() {
        return Err(DockerPullError::BadResponse(format!(
            "config diff_ids count {} does not match manifest layers count {}",
            diff_ids.len(),
            layers.len()
        )));
    }
    let config_path = blob_tar_path(&reference.config_digest)?;
    let layer_descriptors = layers
        .iter()
        .zip(diff_ids.iter())
        .map(|(layer, diff_id)| {
            let digest = layer
                .get("digest")
                .and_then(|value| value.as_str())
                .ok_or_else(|| DockerPullError::BadResponse("layer digest missing".into()))?;
            Ok((digest.to_string(), diff_id.clone()))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut missing_diff_ids = Vec::new();
    for (digest, diff_id) in &layer_descriptors {
        if !store.blob_path(digest)?.exists() {
            missing_diff_ids.push(diff_id.clone());
        }
    }
    let daemon_layers = if missing_diff_ids.is_empty() {
        None
    } else {
        Some(docker::materialize_daemon_layers(store, &missing_diff_ids).await?)
    };
    let layer_sources = layer_descriptors
        .iter()
        .map(|(digest, diff_id)| {
            layer_archive_source(store, daemon_layers.as_ref(), digest, diff_id)
        })
        .collect::<Result<Vec<_>>>()?;

    append_json(
        &mut builder,
        "manifest.json",
        &vec![DockerManifestEntry {
            config: config_path,
            repo_tags: docker_repo_tags(&reference.reference)?,
            layers: layer_sources
                .iter()
                .map(|source| source.archive_path.clone())
                .collect(),
        }],
    )?;

    if let Some(repositories) = docker_repositories(&reference.reference, &reference.config_digest)?
    {
        append_json(&mut builder, "repositories", &repositories)?;
    }

    for source in &layer_sources {
        builder.append_path_with_name(&source.source_path, &source.archive_path)?;
    }

    builder.finish()?;
    Ok(())
}

struct LayerArchiveSource {
    source_path: std::path::PathBuf,
    archive_path: String,
}

fn append_json<W: Write>(
    builder: &mut Builder<W>,
    path: &str,
    value: &impl Serialize,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, path, bytes.as_slice())?;
    Ok(())
}

fn append_blob<W: Write>(builder: &mut Builder<W>, store: &Store, digest: &str) -> Result<()> {
    let path = store.blob_path(digest)?;
    if !path.exists() {
        return Err(DockerPullError::MissingBlobFile(digest.to_string(), path));
    }
    let target = blob_tar_path(digest)?;
    builder.append_path_with_name(path, target)?;
    Ok(())
}

fn layer_archive_source(
    store: &Store,
    daemon_layers: Option<&docker::MaterializedDaemonLayers>,
    digest: &str,
    diff_id: &str,
) -> Result<LayerArchiveSource> {
    let blob_path = store.blob_path(digest)?;
    if blob_path.exists() {
        return Ok(LayerArchiveSource {
            source_path: blob_path,
            archive_path: blob_tar_path(digest)?,
        });
    }

    if let Some(local_path) = daemon_layers.and_then(|layers| layers.path_for(diff_id)) {
        return Ok(LayerArchiveSource {
            source_path: local_path.clone(),
            archive_path: blob_tar_path(diff_id)?,
        });
    }

    Err(DockerPullError::MissingBlobFile(
        digest.to_string(),
        blob_path,
    ))
}

fn blob_tar_path(digest: &str) -> Result<String> {
    let (algorithm, value) = digest
        .split_once(':')
        .ok_or_else(|| DockerPullError::InvalidInput(format!("invalid digest `{digest}`")))?;
    Ok(format!("blobs/{algorithm}/{value}"))
}

fn docker_repo_tags(reference: &str) -> Result<Vec<String>> {
    let parsed = ImageReference::parse(reference)?;
    match parsed.target {
        ReferenceTarget::Tag(_) => Ok(vec![parsed.display_name()]),
        ReferenceTarget::Digest(_) => Ok(Vec::new()),
    }
}

fn oci_ref_name(reference: &str) -> Result<Option<String>> {
    let parsed = ImageReference::parse(reference)?;
    match parsed.target {
        ReferenceTarget::Tag(tag) => Ok(Some(tag)),
        ReferenceTarget::Digest(_) => Ok(None),
    }
}

fn docker_repositories(reference: &str, config_digest: &str) -> Result<Option<serde_json::Value>> {
    let parsed = ImageReference::parse(reference)?;
    let ReferenceTarget::Tag(_) = &parsed.target else {
        return Ok(None);
    };
    let image_name = parsed.display_name();
    let (image_name, tag_name) = image_name.rsplit_once(':').ok_or_else(|| {
        DockerPullError::InvalidInput(format!("invalid display reference `{image_name}`"))
    })?;
    let image_id = config_digest
        .split_once(':')
        .map(|(_, value)| value)
        .ok_or_else(|| {
            DockerPullError::InvalidInput(format!("invalid digest `{config_digest}`"))
        })?;
    let mut tags = serde_json::Map::new();
    tags.insert(
        tag_name.to_string(),
        serde_json::Value::String(image_id.to_string()),
    );
    let mut repositories = serde_json::Map::new();
    repositories.insert(image_name.to_string(), serde_json::Value::Object(tags));
    Ok(Some(serde_json::Value::Object(repositories)))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Cursor, Read};

    use serde_json::Value;
    use sha2::{Digest as _, Sha256};
    use tempfile::tempdir;

    use super::{oci_ref_name, write_oci_archive_to_writer};
    use crate::platform::Platform;
    use crate::registry::Descriptor;
    use crate::store::{Store, StoredReference};

    #[test]
    fn oci_ref_name_uses_tag_only() {
        let ref_name = oci_ref_name("ghcr.io/acme/app:1.2.3").expect("reference should parse");
        assert_eq!(ref_name.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn oci_ref_name_is_omitted_for_digest_references() {
        let ref_name =
            oci_ref_name("ghcr.io/acme/app@sha256:deadbeef").expect("reference should parse");
        assert!(ref_name.is_none());
    }

    #[tokio::test]
    async fn tagged_archive_contains_expected_metadata_entries() {
        let archive = write_test_archive("ghcr.io/acme/app:1.2.3").await;

        let index: Value = serde_json::from_slice(
            archive
                .entries
                .get("index.json")
                .expect("index.json should exist"),
        )
        .expect("index.json should parse");
        assert_eq!(
            index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"],
            "1.2.3"
        );

        let manifest: Value = serde_json::from_slice(
            archive
                .entries
                .get("manifest.json")
                .expect("manifest.json should exist"),
        )
        .expect("manifest.json should parse");
        assert_eq!(
            manifest[0]["RepoTags"],
            serde_json::json!(["ghcr.io/acme/app:1.2.3"])
        );

        let repositories: Value = serde_json::from_slice(
            archive
                .entries
                .get("repositories")
                .expect("repositories should exist for tagged references"),
        )
        .expect("repositories should parse");
        assert_eq!(
            repositories,
            serde_json::json!({
                "ghcr.io/acme/app": {
                    "1.2.3": archive.config_digest.strip_prefix("sha256:").expect("config digest should have prefix")
                }
            })
        );
    }

    #[tokio::test]
    async fn digest_archive_omits_tag_metadata_entries() {
        let archive =
            write_test_archive("ghcr.io/acme/app@sha256:1111111111111111111111111111111111111111111111111111111111111111")
                .await;

        let index: Value = serde_json::from_slice(
            archive
                .entries
                .get("index.json")
                .expect("index.json should exist"),
        )
        .expect("index.json should parse");
        assert!(
            index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"].is_null()
        );

        let manifest: Value = serde_json::from_slice(
            archive
                .entries
                .get("manifest.json")
                .expect("manifest.json should exist"),
        )
        .expect("manifest.json should parse");
        assert_eq!(manifest[0]["RepoTags"], serde_json::json!([]));
        assert!(
            !archive.entries.contains_key("repositories"),
            "digest references should not write repositories metadata"
        );
    }

    struct TestArchive {
        entries: HashMap<String, Vec<u8>>,
        config_digest: String,
    }

    async fn write_test_archive(reference: &str) -> TestArchive {
        let dir = tempdir().expect("tempdir should create");
        let store = Store::open(dir.path().to_path_buf())
            .await
            .expect("store should open");
        let config_bytes = br#"{"rootfs":{"diff_ids":["sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]}}"#;
        let config = Descriptor {
            media_type: "application/vnd.oci.image.config.v1+json".into(),
            digest: digest_bytes(config_bytes),
            size: config_bytes.len() as i64,
            platform: None,
            annotations: None,
        };
        let layer_bytes = b"layer-bytes";
        let layer = Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar".into(),
            digest: digest_bytes(layer_bytes),
            size: layer_bytes.len() as i64,
            platform: None,
            annotations: None,
        };
        let manifest_bytes = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": config.media_type,
                "digest": config.digest,
                "size": config.size
            },
            "layers": [{
                "mediaType": layer.media_type,
                "digest": layer.digest,
                "size": layer.size
            }]
        }))
        .expect("manifest bytes should serialize");
        let manifest = Descriptor {
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            digest: digest_bytes(&manifest_bytes),
            size: manifest_bytes.len() as i64,
            platform: Some(Platform::parse("linux/amd64").expect("platform should parse")),
            annotations: None,
        };

        store
            .save_blob_bytes(&config, config_bytes)
            .await
            .expect("config should be saved");
        store
            .save_blob_bytes(&layer, layer_bytes)
            .await
            .expect("layer should be saved");
        store
            .save_blob_bytes(&manifest, &manifest_bytes)
            .await
            .expect("manifest should be saved");

        let mut archive = Vec::new();
        write_oci_archive_to_writer(
            &mut archive,
            &store,
            &StoredReference {
                reference: reference.to_string(),
                manifest,
                config_digest: config.digest.clone(),
            },
        )
        .await
        .expect("archive should be written");
        TestArchive {
            entries: read_archive_entries(&archive),
            config_digest: config.digest,
        }
    }

    fn read_archive_entries(archive: &[u8]) -> HashMap<String, Vec<u8>> {
        let mut entries = HashMap::new();
        let cursor = Cursor::new(archive);
        let mut tar = tar::Archive::new(cursor);
        for entry in tar.entries().expect("archive entries should open") {
            let mut entry = entry.expect("entry should read");
            let path = entry
                .path()
                .expect("entry path should parse")
                .to_string_lossy()
                .into_owned();
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .expect("entry bytes should read");
            entries.insert(path, bytes);
        }
        entries
    }

    fn digest_bytes(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("sha256:{}", hex::encode(hasher.finalize()))
    }
}
