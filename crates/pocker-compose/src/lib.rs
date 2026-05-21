use std::collections::HashSet;

mod interpolate;
mod project;
mod yaml;

pub use project::resolve_images;

pub type Result<T> = std::result::Result<T, ComposeError>;

#[derive(Debug, thiserror::Error)]
pub enum ComposeError {
    #[error("{0}")]
    InvalidInput(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Yaml(#[from] serde_yml::Error),
}

#[derive(Debug, Clone)]
pub struct ComposeImages {
    pub images: Vec<String>,
    pub skipped_build_only: Vec<String>,
    pub services: Vec<ComposeServiceImage>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ComposeServiceImage {
    pub service: String,
    pub image: Option<String>,
    pub build_only: bool,
}

pub fn unique_images(images: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for image in images {
        if seen.insert(image.clone()) {
            unique.push(image.clone());
        }
    }
    unique
}

pub fn select_services(resolved: &ComposeImages, services: &[String]) -> Result<ComposeImages> {
    if services.is_empty() {
        return Ok(resolved.clone());
    }

    let mut unknown = Vec::new();
    for requested in services {
        if !resolved
            .services
            .iter()
            .any(|service| service.service == *requested)
        {
            unknown.push(requested.clone());
        }
    }
    if !unknown.is_empty() {
        return Err(ComposeError::InvalidInput(format!(
            "compose service(s) not found: {}",
            unknown.join(", ")
        )));
    }

    let requested = services.iter().collect::<HashSet<_>>();
    let services = resolved
        .services
        .iter()
        .filter(|service| requested.contains(&service.service))
        .cloned()
        .collect::<Vec<_>>();
    let images = services
        .iter()
        .filter_map(|service| service.image.clone())
        .collect::<Vec<_>>();
    let skipped_build_only = services
        .iter()
        .filter(|service| service.build_only)
        .map(|service| service.service.clone())
        .collect::<Vec<_>>();

    Ok(ComposeImages {
        images,
        skipped_build_only,
        services,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;

    use tempfile::tempdir;

    use super::{ComposeError, resolve_images, select_services, unique_images};
    use crate::interpolate::interpolate;

    #[test]
    fn resolves_default_file_extends_include_and_env() {
        let dir = tempdir().expect("tempdir should be created");
        fs::write(dir.path().join(".env"), "TAG=1.2.3\n").expect("env should be written");
        fs::write(
            dir.path().join("docker-compose.base.yml"),
            r#"
services:
  base:
    image: example/base:${TAG:-latest}
  child:
    extends:
      service: base
"#,
        )
        .expect("base compose should be written");
        fs::write(
            dir.path().join("docker-compose.include.yml"),
            r#"
services:
  included:
    image: busybox
"#,
        )
        .expect("include compose should be written");
        fs::write(
            dir.path().join("docker-compose.yml"),
            r#"
include:
  - docker-compose.include.yml
services:
  app:
    extends:
      file: docker-compose.base.yml
      service: child
  worker:
    extends:
      service: app
  build_only:
    build: .
"#,
        )
        .expect("root compose should be written");

        let resolved = resolve_images(&[], dir.path()).expect("images should resolve");

        assert_eq!(
            resolved.images,
            vec![
                "example/base:1.2.3".to_string(),
                "example/base:1.2.3".to_string(),
                "busybox".to_string(),
            ]
        );
        assert_eq!(resolved.skipped_build_only, vec!["build_only".to_string()]);

        let selected =
            select_services(&resolved, &["included".to_string()]).expect("service should select");
        assert_eq!(selected.images, vec!["busybox".to_string()]);
        assert_eq!(selected.services[0].service, "included");
    }

    #[test]
    fn deduplicates_images_in_first_seen_order() {
        let images = unique_images(&[
            "alpine:latest".into(),
            "busybox:latest".into(),
            "alpine:latest".into(),
        ]);

        assert_eq!(images, vec!["alpine:latest", "busybox:latest"]);
    }

    #[test]
    fn interpolates_unbraced_compose_variables() {
        let values = HashMap::from([
            ("REGISTRY".to_string(), "example.com".to_string()),
            ("IMAGE_TAG".to_string(), "1.2.3".to_string()),
            ("EMPTY".to_string(), String::new()),
        ]);

        let interpolated = interpolate(
            "image: $REGISTRY/app:$IMAGE_TAG\nfallback: ${EMPTY:-latest}\nescaped: $$IMAGE_TAG\nliteral: $-",
            &values,
        )
        .expect("compose interpolation should succeed");

        assert_eq!(
            interpolated,
            "image: example.com/app:1.2.3\nfallback: latest\nescaped: $IMAGE_TAG\nliteral: $-"
        );
    }

    #[test]
    fn interpolation_operators_match_compose_presence_rules() {
        let values = HashMap::from([
            ("SET".to_string(), "value".to_string()),
            ("EMPTY".to_string(), String::new()),
        ]);

        let interpolated = interpolate(
            concat!(
                "default_empty: ${EMPTY:-fallback}\n",
                "default_unset: ${UNSET-fallback}\n",
                "keep_empty: ${EMPTY-fallback}\n",
                "alt_set: ${SET:+enabled}\n",
                "alt_empty: ${EMPTY+enabled}\n",
                "plain_unset: ${UNSET}\n",
            ),
            &values,
        )
        .expect("compose interpolation should succeed");

        assert_eq!(
            interpolated,
            concat!(
                "default_empty: fallback\n",
                "default_unset: fallback\n",
                "keep_empty: \n",
                "alt_set: enabled\n",
                "alt_empty: enabled\n",
                "plain_unset: \n",
            )
        );
    }

    #[test]
    fn required_variable_operator_wins_over_dash_in_message() {
        let error = interpolate("${REQUIRED?-must be set}", &HashMap::new())
            .expect_err("required operator should fail when variable is unset");

        assert!(matches!(
            error,
            ComposeError::InvalidInput(message)
                if message == "compose variable `REQUIRED` is required: -must be set"
        ));
    }

    #[test]
    fn repeated_compose_files_merge_service_overrides() {
        let dir = tempdir().expect("tempdir should be created");
        let base = dir.path().join("compose.yml");
        let override_file = dir.path().join("compose.override.yml");
        fs::write(
            &base,
            r#"
services:
  app:
    image: example/app:1.0
  worker:
    image: example/worker:1.0
"#,
        )
        .expect("base compose should be written");
        fs::write(
            &override_file,
            r#"
services:
  app:
    image: example/app:2.0
"#,
        )
        .expect("override compose should be written");

        let resolved = resolve_images(&[base, override_file], dir.path())
            .expect("compose files should resolve");

        assert_eq!(
            resolved.images,
            vec![
                "example/app:2.0".to_string(),
                "example/worker:1.0".to_string()
            ]
        );
        assert_eq!(resolved.services[0].service, "app");
        assert_eq!(
            resolved.services[0].image,
            Some("example/app:2.0".to_string())
        );
    }

    #[test]
    fn yaml_merge_anchors_are_considered_when_resolving_images() {
        let dir = tempdir().expect("tempdir should be created");
        fs::write(dir.path().join(".env"), "TAG=3.1\n").expect("env should be written");
        fs::write(
            dir.path().join("compose.yml"),
            r#"
x-image-defaults: &image-defaults
  image: example/app:${TAG:-latest}

services:
  app:
    <<: *image-defaults
"#,
        )
        .expect("compose should be written");

        let resolved = resolve_images(&[], dir.path()).expect("compose file should resolve");

        assert_eq!(resolved.images, vec!["example/app:3.1".to_string()]);
        assert_eq!(resolved.services[0].service, "app");
    }

    #[test]
    fn profiled_services_are_still_discovered_by_config_parser() {
        let dir = tempdir().expect("tempdir should be created");
        fs::write(
            dir.path().join("compose.yml"),
            r#"
services:
  default:
    image: example/default:latest
  optional:
    profiles:
      - tools
    image: example/optional:latest
"#,
        )
        .expect("compose should be written");

        let resolved = resolve_images(&[], dir.path()).expect("compose file should resolve");

        assert_eq!(
            resolved.images,
            vec![
                "example/default:latest".to_string(),
                "example/optional:latest".to_string(),
            ]
        );
    }
}
