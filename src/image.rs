use serde::Deserialize;

use crate::error::{DockerPullError, Result};
use crate::registry::Descriptor;

#[derive(Debug, Clone)]
pub struct LayerSpec {
    pub descriptor: Descriptor,
    pub diff_id: String,
}

#[derive(Debug, Deserialize)]
struct ImageConfig {
    rootfs: RootFs,
}

#[derive(Debug, Deserialize)]
struct RootFs {
    #[serde(default, rename = "diff_ids")]
    diff_ids: Vec<String>,
}

pub fn parse_diff_ids(config_bytes: &[u8]) -> Result<Vec<String>> {
    let config: ImageConfig = serde_json::from_slice(config_bytes)?;
    Ok(config.rootfs.diff_ids)
}

pub fn pair_layers(layers: Vec<Descriptor>, config_bytes: &[u8]) -> Result<Vec<LayerSpec>> {
    let diff_ids = parse_diff_ids(config_bytes)?;
    if diff_ids.len() != layers.len() {
        return Err(DockerPullError::BadResponse(format!(
            "config diff_ids count {} does not match manifest layers count {}",
            diff_ids.len(),
            layers.len()
        )));
    }

    Ok(layers
        .into_iter()
        .zip(diff_ids)
        .map(|(descriptor, diff_id)| LayerSpec {
            descriptor,
            diff_id,
        })
        .collect())
}
