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

    pub fn local_repository(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
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

#[cfg(test)]
mod tests {
    use super::{ImageReference, ReferenceTarget};

    #[test]
    fn docker_hub_defaults_are_applied() {
        let reference = ImageReference::parse("alpine").expect("reference should parse");
        assert_eq!(reference.registry, "registry-1.docker.io");
        assert_eq!(reference.repository, "library/alpine");
        assert_eq!(reference.target, ReferenceTarget::Tag("latest".into()));
    }

    #[test]
    fn digest_reference_is_preserved() {
        let reference = ImageReference::parse("ghcr.io/acme/app@sha256:deadbeef")
            .expect("reference should parse");
        assert_eq!(reference.registry, "ghcr.io");
        assert_eq!(reference.repository, "acme/app");
        assert_eq!(
            reference.target,
            ReferenceTarget::Digest("sha256:deadbeef".into())
        );
    }

    #[test]
    fn docker_hub_display_name_is_short() {
        let reference = ImageReference::parse("alpine").expect("reference should parse");
        assert_eq!(reference.display_name(), "alpine:latest");
    }
}
