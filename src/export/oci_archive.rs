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
    let ref_name = reference
        .reference
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .unwrap_or(&reference.reference);
    manifest_descriptor.annotations = Some(
        [(
            "org.opencontainers.image.ref.name".to_string(),
            ref_name.to_string(),
        )]
        .into_iter()
        .collect(),
    );
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
