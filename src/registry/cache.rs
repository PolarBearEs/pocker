use url::Url;

use crate::error::{DockerPullError, Result};
use crate::reference::ImageReference;

pub(crate) fn cache_repository(registry: &str, repository: &str) -> String {
    format!("{registry}/{repository}")
}

pub(crate) fn decode_cache_repository(repository: &str) -> Result<(String, String)> {
    let mut parts = repository.splitn(2, '/');
    let Some(registry) = parts.next() else {
        return Err(invalid_cache_repository(repository));
    };
    let Some(upstream_repository) = parts.next() else {
        return Err(invalid_cache_repository(repository));
    };
    if registry.is_empty() || upstream_repository.is_empty() {
        return Err(invalid_cache_repository(repository));
    }
    Ok((registry.to_string(), upstream_repository.to_string()))
}

fn invalid_cache_repository(repository: &str) -> DockerPullError {
    DockerPullError::InvalidInput(format!("invalid pocker cache repository `{repository}`"))
}

pub(crate) fn cache_url(
    base_url: &Url,
    reference: &ImageReference,
    resource: &str,
    suffix: &str,
) -> Result<Url> {
    let repository = cache_repository(&reference.registry, &reference.repository);
    resource_url(base_url.clone(), &repository, resource, suffix)
}

pub(crate) fn resource_url(
    mut base_url: Url,
    repository: &str,
    resource: &str,
    suffix: &str,
) -> Result<Url> {
    base_url.set_query(None);
    base_url.set_fragment(None);
    {
        let display_base = base_url.to_string();
        let mut segments = base_url.path_segments_mut().map_err(|_| {
            DockerPullError::InvalidInput(format!("invalid registry URL base `{display_base}`"))
        })?;
        segments.push("v2");
        for segment in repository.split('/') {
            segments.push(segment);
        }
        segments.push(resource);
        segments.push(suffix);
    }
    Ok(base_url)
}
