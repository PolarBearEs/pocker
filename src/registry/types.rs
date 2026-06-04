use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::{DockerPullError, Result};
use crate::platform::Platform;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    pub digest: String,
    pub size: i64,
    #[serde(default)]
    pub platform: Option<Platform>,
    #[serde(default)]
    pub annotations: Option<HashMap<String, String>>,
}

impl Descriptor {
    pub(crate) fn expected_size(&self) -> Result<u64> {
        u64::try_from(self.size).map_err(|_| {
            DockerPullError::InvalidInput(format!(
                "descriptor {} has invalid negative size {}",
                self.digest, self.size
            ))
        })
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ManifestEnvelope {
    #[serde(rename = "schemaVersion")]
    pub(crate) schema_version: u32,
    #[serde(rename = "mediaType", default)]
    pub(crate) media_type: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ImageManifest {
    #[serde(rename = "mediaType", default)]
    pub(crate) media_type: String,
    pub(crate) config: Descriptor,
    pub(crate) layers: Vec<Descriptor>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ImageIndex {
    pub(crate) manifests: Vec<Descriptor>,
}

#[derive(Debug, Clone)]
pub struct ResolvedImage {
    pub manifest: Descriptor,
    pub manifest_bytes: Vec<u8>,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

#[derive(Debug)]
pub struct BlobMetadata {
    pub size: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RawManifest {
    pub descriptor: Descriptor,
    pub bytes: Vec<u8>,
}
