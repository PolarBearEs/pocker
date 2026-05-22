use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tar::{Builder, Header};

use crate::digest::{canonical_digest_bytes, digest_hex, parse_digest};
use crate::docker;
use crate::error::{DockerPullError, Result};
use crate::image::parse_diff_ids;
use crate::reference::{ImageReference, ReferenceTarget};
use crate::registry::Descriptor;
use crate::store::Store;
use crate::store::StoredReference;

const OCI_IMAGE_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OciManifest {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "mediaType", default)]
    media_type: String,
    config: Descriptor,
    layers: Vec<Descriptor>,
}

struct ArchiveInputs {
    manifest: OciManifest,
    diff_ids: Vec<String>,
    missing_diff_ids: Vec<String>,
}

pub(crate) struct PreparedOciArchive {
    inputs: ArchiveInputs,
    daemon_layers: Option<docker::MaterializedDaemonLayers>,
}

#[cfg(test)]
pub async fn write_oci_archive_to_writer<W: Write>(
    writer: W,
    store: &Store,
    reference: &StoredReference,
) -> Result<()> {
    let prepared = prepare_oci_archive(store, reference).await?;
    write_prepared_oci_archive_to_writer(writer, store, reference, &prepared)
}

pub(crate) async fn prepare_oci_archive(
    store: &Store,
    reference: &StoredReference,
) -> Result<PreparedOciArchive> {
    let inputs = load_archive_inputs(store, reference)?;
    let daemon_layers = if inputs.missing_diff_ids.is_empty() {
        None
    } else {
        Some(docker::materialize_daemon_layers(store, &inputs.missing_diff_ids).await?)
    };
    Ok(PreparedOciArchive {
        inputs,
        daemon_layers,
    })
}

pub(crate) fn write_prepared_oci_archive_to_writer<W: Write>(
    writer: W,
    store: &Store,
    reference: &StoredReference,
    prepared: &PreparedOciArchive,
) -> Result<()> {
    write_oci_archive_to_writer_with_fallbacks(
        writer,
        store,
        reference,
        &prepared.inputs,
        prepared
            .daemon_layers
            .as_ref()
            .map(docker::MaterializedDaemonLayers::paths),
    )
}

fn write_oci_archive_to_writer_with_fallbacks<W: Write>(
    writer: W,
    store: &Store,
    reference: &StoredReference,
    inputs: &ArchiveInputs,
    fallback_paths: Option<&HashMap<String, PathBuf>>,
) -> Result<()> {
    let mut builder = Builder::new(writer);
    append_json(
        &mut builder,
        "oci-layout",
        &OciLayout {
            image_layout_version: "1.0.0",
        },
    )?;

    let config_path = blob_tar_path(&reference.config_digest)?;
    let layer_descriptors = inputs
        .manifest
        .layers
        .iter()
        .cloned()
        .zip(inputs.diff_ids.iter().cloned())
        .collect::<Vec<_>>();
    let layer_sources = layer_descriptors
        .iter()
        .map(|(layer, diff_id)| layer_archive_source(store, fallback_paths, layer, diff_id))
        .collect::<Result<Vec<_>>>()?;
    let archive_manifest = archive_manifest(&inputs.manifest, &layer_sources, &reference.manifest);
    let archive_manifest_bytes = serde_json::to_vec(&archive_manifest)?;
    let mut manifest_descriptor = reference.manifest.clone();
    manifest_descriptor.digest = canonical_digest_bytes(&archive_manifest_bytes);
    manifest_descriptor.size = archive_manifest_bytes.len() as i64;
    if manifest_descriptor.media_type.is_empty() {
        manifest_descriptor.media_type = archive_manifest.media_type.clone();
    }
    let parsed_reference = ImageReference::parse(&reference.reference)?;
    if let Some(ref_name) = oci_ref_name(&parsed_reference) {
        let mut annotations = manifest_descriptor.annotations.unwrap_or_default();
        annotations.insert("org.opencontainers.image.ref.name".to_string(), ref_name);
        manifest_descriptor.annotations = Some(annotations);
    }

    append_json(
        &mut builder,
        "index.json",
        &IndexJson {
            schema_version: 2,
            manifests: vec![manifest_descriptor.clone()],
        },
    )?;
    append_blob_bytes(
        &mut builder,
        &manifest_descriptor.digest,
        &archive_manifest_bytes,
    )?;
    append_blob(&mut builder, store, &reference.config_digest)?;

    append_json(
        &mut builder,
        "manifest.json",
        &vec![DockerManifestEntry {
            config: config_path,
            repo_tags: docker_repo_tags(&parsed_reference),
            layers: layer_sources
                .iter()
                .map(|source| source.archive_path.clone())
                .collect(),
        }],
    )?;

    if let Some(repositories) = docker_repositories(&parsed_reference, &reference.config_digest)? {
        append_json(&mut builder, "repositories", &repositories)?;
    }

    for source in &layer_sources {
        builder.append_path_with_name(&source.source_path, &source.archive_path)?;
    }

    builder.finish()?;
    Ok(())
}

fn load_archive_inputs(store: &Store, reference: &StoredReference) -> Result<ArchiveInputs> {
    let manifest_path = store.blob_path(&reference.manifest.digest)?;
    let manifest_bytes = std::fs::read(manifest_path)?;
    let config_blob_path = store.blob_path(&reference.config_digest)?;
    let config_bytes = std::fs::read(config_blob_path)?;
    let manifest: OciManifest = serde_json::from_slice(&manifest_bytes)?;
    let diff_ids = parse_diff_ids(&config_bytes)?;
    if diff_ids.len() != manifest.layers.len() {
        return Err(DockerPullError::BadResponse(format!(
            "config diff_ids count {} does not match manifest layers count {}",
            diff_ids.len(),
            manifest.layers.len()
        )));
    }

    let mut missing = Vec::new();
    let mut resolved_diff_ids = Vec::with_capacity(diff_ids.len());
    for (layer, diff_id) in manifest.layers.iter().zip(diff_ids) {
        if !store.blob_path(&layer.digest)?.exists() {
            missing.push(diff_id.clone());
        }
        resolved_diff_ids.push(diff_id);
    }
    Ok(ArchiveInputs {
        manifest,
        diff_ids: resolved_diff_ids,
        missing_diff_ids: missing,
    })
}

struct LayerArchiveSource {
    descriptor: Descriptor,
    source_path: PathBuf,
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

fn append_blob_bytes<W: Write>(builder: &mut Builder<W>, digest: &str, bytes: &[u8]) -> Result<()> {
    let target = blob_tar_path(digest)?;
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, target, bytes)?;
    Ok(())
}

fn layer_archive_source(
    store: &Store,
    fallback_paths: Option<&HashMap<String, PathBuf>>,
    descriptor: &Descriptor,
    diff_id: &str,
) -> Result<LayerArchiveSource> {
    let blob_path = store.blob_path(&descriptor.digest)?;
    if blob_path.exists() {
        return Ok(LayerArchiveSource {
            descriptor: descriptor.clone(),
            source_path: blob_path,
            archive_path: blob_tar_path(&descriptor.digest)?,
        });
    }

    if let Some(local_path) = fallback_paths.and_then(|paths| paths.get(diff_id)) {
        return Ok(LayerArchiveSource {
            descriptor: Descriptor {
                media_type: "application/vnd.oci.image.layer.v1.tar".into(),
                digest: diff_id.to_string(),
                size: std::fs::metadata(local_path)?.len() as i64,
                platform: None,
                annotations: None,
            },
            source_path: local_path.clone(),
            archive_path: blob_tar_path(diff_id)?,
        });
    }

    Err(DockerPullError::MissingBlobFile(
        descriptor.digest.to_string(),
        blob_path,
    ))
}

fn archive_manifest(
    manifest: &OciManifest,
    layer_sources: &[LayerArchiveSource],
    manifest_descriptor: &Descriptor,
) -> OciManifest {
    OciManifest {
        schema_version: manifest.schema_version,
        media_type: resolved_manifest_media_type(manifest, manifest_descriptor),
        config: manifest.config.clone(),
        layers: layer_sources
            .iter()
            .map(|source| source.descriptor.clone())
            .collect(),
    }
}

fn resolved_manifest_media_type(
    manifest: &OciManifest,
    manifest_descriptor: &Descriptor,
) -> String {
    if !manifest.media_type.is_empty() {
        manifest.media_type.clone()
    } else if !manifest_descriptor.media_type.is_empty() {
        manifest_descriptor.media_type.clone()
    } else {
        OCI_IMAGE_MANIFEST_MEDIA_TYPE.to_string()
    }
}

fn blob_tar_path(digest: &str) -> Result<String> {
    let parsed = parse_digest(digest)?;
    Ok(format!("blobs/{}/{}", parsed.algorithm, parsed.value))
}

fn docker_repo_tags(reference: &ImageReference) -> Vec<String> {
    match &reference.target {
        ReferenceTarget::Tag(_) => vec![reference.display_name()],
        ReferenceTarget::Digest(_) => Vec::new(),
    }
}

fn oci_ref_name(reference: &ImageReference) -> Option<String> {
    match &reference.target {
        ReferenceTarget::Tag(tag) => Some(tag.clone()),
        ReferenceTarget::Digest(_) => None,
    }
}

fn docker_repositories(
    reference: &ImageReference,
    config_digest: &str,
) -> Result<Option<serde_json::Value>> {
    let ReferenceTarget::Tag(_) = &reference.target else {
        return Ok(None);
    };
    let image_name = reference.display_name();
    let (image_name, tag_name) = image_name.rsplit_once(':').ok_or_else(|| {
        DockerPullError::InvalidInput(format!("invalid display reference `{image_name}`"))
    })?;
    let image_id = digest_hex(config_digest)?;
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
    use tempfile::tempdir;

    use super::{
        OCI_IMAGE_MANIFEST_MEDIA_TYPE, blob_tar_path, load_archive_inputs, oci_ref_name,
        write_oci_archive_to_writer, write_oci_archive_to_writer_with_fallbacks,
    };
    use crate::digest::canonical_digest_bytes;
    use crate::platform::Platform;
    use crate::reference::ImageReference;
    use crate::registry::Descriptor;
    use crate::store::{Store, StoredReference};

    #[test]
    fn oci_ref_name_uses_tag_only() {
        let reference =
            ImageReference::parse("ghcr.io/acme/app:1.2.3").expect("reference should parse");
        let ref_name = oci_ref_name(&reference);
        assert_eq!(ref_name.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn oci_ref_name_is_omitted_for_digest_references() {
        let reference = ImageReference::parse(
            "ghcr.io/acme/app@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .expect("reference should parse");
        let ref_name = oci_ref_name(&reference);
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

    #[tokio::test]
    async fn daemon_layer_fallback_rewrites_oci_manifest_to_materialized_digest() {
        let dir = tempdir().expect("tempdir should create");
        let store = Store::open(dir.path().to_path_buf())
            .await
            .expect("store should open");

        let materialized_layer_bytes = b"materialized-layer-bytes";
        let diff_id = digest_bytes(materialized_layer_bytes);
        let config_bytes = format!(r#"{{"rootfs":{{"diff_ids":["{diff_id}"]}}}}"#).into_bytes();
        let config = Descriptor {
            media_type: "application/vnd.oci.image.config.v1+json".into(),
            digest: digest_bytes(&config_bytes),
            size: config_bytes.len() as i64,
            platform: None,
            annotations: None,
        };
        let original_layer = Descriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
            digest: digest_bytes(b"compressed-layer-bytes"),
            size: 22,
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
                "mediaType": original_layer.media_type,
                "digest": original_layer.digest,
                "size": original_layer.size
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
            .save_blob_bytes(&config, &config_bytes)
            .await
            .expect("config should be saved");
        store
            .save_blob_bytes(&manifest, &manifest_bytes)
            .await
            .expect("manifest should be saved");

        let fallback_path = dir.path().join("materialized-layer.tar");
        std::fs::write(&fallback_path, materialized_layer_bytes)
            .expect("materialized layer should be written");
        let fallback_paths = HashMap::from([(diff_id.clone(), fallback_path)]);
        let stored_reference = StoredReference {
            reference: "ghcr.io/acme/app:1.2.3".to_string(),
            manifest,
            config_digest: config.digest.clone(),
        };
        let inputs =
            load_archive_inputs(&store, &stored_reference).expect("archive inputs should load");

        let mut archive = Vec::new();
        write_oci_archive_to_writer_with_fallbacks(
            &mut archive,
            &store,
            &stored_reference,
            &inputs,
            Some(&fallback_paths),
        )
        .expect("archive should be written");

        let entries = read_archive_entries(&archive);
        let index: Value =
            serde_json::from_slice(entries.get("index.json").expect("index.json should exist"))
                .expect("index.json should parse");
        let rewritten_manifest_digest = index["manifests"][0]["digest"]
            .as_str()
            .expect("index manifest digest should be a string");
        let rewritten_manifest_path =
            blob_tar_path(rewritten_manifest_digest).expect("manifest path should build");
        let rewritten_manifest: Value = serde_json::from_slice(
            entries
                .get(&rewritten_manifest_path)
                .expect("rewritten manifest blob should exist"),
        )
        .expect("rewritten manifest should parse");

        assert_eq!(rewritten_manifest["layers"][0]["digest"], diff_id);
        assert_eq!(
            rewritten_manifest["layers"][0]["mediaType"],
            "application/vnd.oci.image.layer.v1.tar"
        );
        assert_eq!(
            rewritten_manifest["layers"][0]["size"],
            materialized_layer_bytes.len() as i64
        );
        assert!(
            entries.contains_key(&blob_tar_path(&diff_id).expect("layer path should build")),
            "materialized layer should be archived at its content digest path"
        );
    }

    #[tokio::test]
    async fn missing_manifest_media_type_defaults_to_oci_in_manifest_and_index() {
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
            media_type: String::new(),
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

        let stored_reference = StoredReference {
            reference: "ghcr.io/acme/app:1.2.3".to_string(),
            manifest,
            config_digest: config.digest.clone(),
        };
        let archive = write_test_archive_for_reference(&store, &stored_reference).await;
        let index: Value = serde_json::from_slice(
            archive
                .entries
                .get("index.json")
                .expect("index.json should exist"),
        )
        .expect("index.json should parse");
        let manifest_digest = index["manifests"][0]["digest"]
            .as_str()
            .expect("manifest digest should be present");
        assert_eq!(
            index["manifests"][0]["mediaType"],
            OCI_IMAGE_MANIFEST_MEDIA_TYPE
        );

        let manifest_path = blob_tar_path(manifest_digest).expect("manifest path should build");
        let rewritten_manifest: Value = serde_json::from_slice(
            archive
                .entries
                .get(&manifest_path)
                .expect("rewritten manifest should exist"),
        )
        .expect("rewritten manifest should parse");
        assert_eq!(
            rewritten_manifest["mediaType"],
            OCI_IMAGE_MANIFEST_MEDIA_TYPE
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

        let stored_reference = StoredReference {
            reference: reference.to_string(),
            manifest,
            config_digest: config.digest.clone(),
        };
        write_test_archive_for_reference(&store, &stored_reference).await
    }

    async fn write_test_archive_for_reference(
        store: &Store,
        stored_reference: &StoredReference,
    ) -> TestArchive {
        let mut archive = Vec::new();
        write_oci_archive_to_writer(&mut archive, store, stored_reference)
            .await
            .expect("archive should be written");
        TestArchive {
            entries: read_archive_entries(&archive),
            config_digest: stored_reference.config_digest.clone(),
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
        canonical_digest_bytes(bytes)
    }
}
