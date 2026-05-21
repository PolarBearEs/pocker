use crate::digest::parse_digest;
use crate::error::{DockerPullError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceTarget {
    Tag(String),
    Digest(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReference {
    pub registry: String,
    pub repository: String,
    pub target: ReferenceTarget,
}

impl ImageReference {
    pub fn parse(input: &str) -> Result<Self> {
        let mut registry = "registry-1.docker.io".to_string();
        let mut rest = input.trim();
        if rest.is_empty() {
            return Err(DockerPullError::InvalidInput(
                "image reference is empty".into(),
            ));
        }

        if let Some((candidate, suffix)) = split_registry(rest) {
            registry = candidate.to_string();
            rest = suffix;
        }

        let (repository, target) = if let Some((repo, digest)) = rest.rsplit_once('@') {
            (
                repo.to_string(),
                ReferenceTarget::Digest(digest.to_string()),
            )
        } else if let Some((repo, tag)) = split_tag(rest) {
            (repo.to_string(), ReferenceTarget::Tag(tag.to_string()))
        } else {
            (rest.to_string(), ReferenceTarget::Tag("latest".to_string()))
        };

        if repository.is_empty() {
            return Err(DockerPullError::InvalidInput("repository is empty".into()));
        }
        validate_repository(&repository)?;
        validate_target(&target)?;

        let repository = if registry == "registry-1.docker.io" && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };

        Ok(Self {
            registry,
            repository,
            target,
        })
    }

    pub fn normalized(&self) -> String {
        match &self.target {
            ReferenceTarget::Tag(tag) => format!("{}/{}:{tag}", self.registry, self.repository),
            ReferenceTarget::Digest(digest) => {
                format!("{}/{}@{digest}", self.registry, self.repository)
            }
        }
    }

    pub fn manifest_reference(&self) -> &str {
        match &self.target {
            ReferenceTarget::Tag(tag) => tag,
            ReferenceTarget::Digest(digest) => digest,
        }
    }

    pub fn repository_scope(&self) -> String {
        format!("repository:{}:pull", self.repository)
    }

    pub fn display_name(&self) -> String {
        if is_docker_hub(&self.registry) {
            let repository = self
                .repository
                .strip_prefix("library/")
                .unwrap_or(&self.repository);
            return self.with_repository(repository);
        }

        self.normalized()
    }

    fn with_repository(&self, repository: &str) -> String {
        match &self.target {
            ReferenceTarget::Tag(tag) => format!("{repository}:{tag}"),
            ReferenceTarget::Digest(digest) => format!("{repository}@{digest}"),
        }
    }
}

fn is_docker_hub(registry: &str) -> bool {
    matches!(
        registry,
        "registry-1.docker.io" | "docker.io" | "index.docker.io"
    )
}

fn split_registry(input: &str) -> Option<(&str, &str)> {
    let (first, rest) = input.split_once('/')?;
    if first.contains('.') || first.contains(':') || first == "localhost" {
        Some((first, rest))
    } else {
        None
    }
}

fn split_tag(input: &str) -> Option<(&str, &str)> {
    let slash = input.rfind('/');
    let colon = input.rfind(':')?;
    if slash.is_some_and(|slash_index| colon < slash_index) {
        None
    } else {
        Some((&input[..colon], &input[colon + 1..]))
    }
}

fn validate_target(target: &ReferenceTarget) -> Result<()> {
    match target {
        ReferenceTarget::Tag(tag) => validate_tag(tag),
        ReferenceTarget::Digest(digest) => parse_digest(digest).map(|_| ()),
    }
}

fn validate_tag(tag: &str) -> Result<()> {
    let valid = !tag.is_empty()
        && tag.len() <= 128
        && tag
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(DockerPullError::InvalidInput(format!(
            "invalid image tag `{tag}`"
        )))
    }
}

fn validate_repository(repository: &str) -> Result<()> {
    if repository
        .split('/')
        .all(|component| !component.is_empty() && valid_repository_component(component))
    {
        Ok(())
    } else {
        Err(DockerPullError::InvalidInput(format!(
            "invalid repository `{repository}`"
        )))
    }
}

fn valid_repository_component(component: &str) -> bool {
    component.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
    }) && component
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && component
            .bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::{ImageReference, ReferenceTarget};

    const VALID_SHA256: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn docker_hub_defaults_are_applied() {
        let reference = ImageReference::parse("alpine").expect("reference should parse");
        assert_eq!(reference.registry, "registry-1.docker.io");
        assert_eq!(reference.repository, "library/alpine");
        assert_eq!(reference.target, ReferenceTarget::Tag("latest".into()));
    }

    #[test]
    fn digest_reference_is_preserved() {
        let reference = ImageReference::parse(&format!("ghcr.io/acme/app@{VALID_SHA256}"))
            .expect("reference should parse");
        assert_eq!(reference.registry, "ghcr.io");
        assert_eq!(reference.repository, "acme/app");
        assert_eq!(
            reference.target,
            ReferenceTarget::Digest(VALID_SHA256.into())
        );
    }

    #[test]
    fn docker_hub_display_name_is_short() {
        let reference = ImageReference::parse("alpine").expect("reference should parse");
        assert_eq!(reference.display_name(), "alpine:latest");
    }

    #[test]
    fn rejects_empty_tag() {
        ImageReference::parse("alpine:").expect_err("empty tag should be rejected");
    }

    #[test]
    fn rejects_empty_digest() {
        ImageReference::parse("alpine@").expect_err("empty digest should be rejected");
    }

    #[test]
    fn rejects_malformed_digest() {
        ImageReference::parse("alpine@sha256:deadbeef")
            .expect_err("short digest should be rejected");
    }

    #[test]
    fn rejects_invalid_repository_component() {
        ImageReference::parse("ghcr.io/acme//app:latest")
            .expect_err("empty repository component should be rejected");
        ImageReference::parse("ghcr.io/Acme/app:latest")
            .expect_err("uppercase repository component should be rejected");
    }

    #[test]
    fn rejects_invalid_tag_characters() {
        ImageReference::parse("ghcr.io/acme/app:bad/tag")
            .expect_err("tag with slash should be rejected");
        ImageReference::parse("alpine:-bad").expect_err("tag must start with word character");
    }
}
