pub(crate) mod cache;
mod client;
mod types;

pub(crate) const OCI_IMAGE_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";
pub(crate) const DOCKER_MANIFEST_LIST_MEDIA_TYPE: &str =
    "application/vnd.docker.distribution.manifest.list.v2+json";
pub(crate) const OCI_IMAGE_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
pub(crate) const DOCKER_IMAGE_MANIFEST_MEDIA_TYPE: &str =
    "application/vnd.docker.distribution.manifest.v2+json";
pub(crate) const OCI_IMAGE_LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar";
pub(crate) const OCTET_STREAM_MEDIA_TYPE: &str = "application/octet-stream";

pub(crate) const MANIFEST_ACCEPT: &str = concat!(
    "application/vnd.oci.image.index.v1+json,",
    "application/vnd.docker.distribution.manifest.list.v2+json,",
    "application/vnd.oci.image.manifest.v1+json,",
    "application/vnd.docker.distribution.manifest.v2+json"
);

pub(crate) use cache::{cache_repository, decode_cache_repository};
pub(crate) use client::{DEFAULT_REQUEST_RETRIES, RegistryClient};
pub(crate) use types::{Descriptor, ResolvedImage};
