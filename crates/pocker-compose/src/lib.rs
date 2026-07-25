use std::collections::{BTreeMap, HashSet};

use serde::Serialize;

mod env;
mod interpolate;
mod project;
mod yaml;

pub use project::{resolve_images, resolve_images_with_profiles};

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

#[derive(Debug, Clone, Serialize)]
pub struct ComposeImages {
    pub services: Vec<ComposeServiceImage>,
    pub images: Vec<String>,
    pub skipped_build_only: Vec<String>,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct ComposeServiceImage {
    #[serde(rename = "name")]
    pub service: String,
    pub image: Option<String>,
    pub build_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, Option<String>>>,
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

    use super::{
        ComposeError, resolve_images, resolve_images_with_profiles, select_services, unique_images,
    };
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
    fn resolves_default_override_file_when_no_files_are_given() {
        let dir = tempdir().expect("tempdir should be created");
        fs::write(
            dir.path().join("docker-compose.yml"),
            r#"
services:
  app:
    image: example/app:base
  worker:
    image: example/worker:base
"#,
        )
        .expect("base compose should be written");
        fs::write(
            dir.path().join("docker-compose.override.yml"),
            r#"
services:
  app:
    image: example/app:override
"#,
        )
        .expect("override compose should be written");

        let resolved = resolve_images(&[], dir.path()).expect("images should resolve");

        assert_eq!(
            resolved.images,
            vec!["example/app:override", "example/worker:base"]
        );
    }

    #[test]
    fn uses_first_default_override_file_by_compose_preference() {
        let dir = tempdir().expect("tempdir should be created");
        fs::write(
            dir.path().join("compose.yaml"),
            r#"
services:
  app:
    image: example/app:base
"#,
        )
        .expect("base compose should be written");
        fs::write(
            dir.path().join("compose.override.yml"),
            r#"
services:
  app:
    image: example/app:yml
"#,
        )
        .expect("preferred override should be written");
        fs::write(
            dir.path().join("compose.override.yaml"),
            r#"
services:
  app:
    image: example/app:yaml
"#,
        )
        .expect("lower-priority override should be written");

        let resolved = resolve_images(&[], dir.path()).expect("images should resolve");

        assert_eq!(resolved.images, vec!["example/app:yml"]);
    }

    #[test]
    fn discovers_default_compose_files_from_parent_directories() {
        let dir = tempdir().expect("tempdir should be created");
        let child = dir.path().join("nested").join("child");
        fs::create_dir_all(&child).expect("child dir should be created");
        fs::write(
            dir.path().join("compose.yaml"),
            r#"
services:
  app:
    image: example/app:parent
"#,
        )
        .expect("base compose should be written");
        fs::write(
            dir.path().join("compose.override.yml"),
            r#"
services:
  app:
    image: example/app:parent-override
"#,
        )
        .expect("override compose should be written");

        let resolved = resolve_images(&[], &child).expect("images should resolve");

        assert_eq!(resolved.images, vec!["example/app:parent-override"]);
    }

    #[test]
    fn detects_build_only_services_from_yaml_merge_keys() {
        let dir = tempdir().expect("tempdir should be created");
        fs::write(
            dir.path().join("docker-compose.yml"),
            r#"
x-build: &build
  build: .
services:
  builder:
    <<: *build
"#,
        )
        .expect("compose should be written");

        let resolved = resolve_images(&[], dir.path()).expect("images should resolve");

        assert_eq!(resolved.images, Vec::<String>::new());
        assert_eq!(resolved.skipped_build_only, vec!["builder"]);
        assert_eq!(resolved.services[0].service, "builder");
        assert!(resolved.services[0].build_only);
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
    fn nested_interpolation_matches_compose_forms() {
        let values = HashMap::from([
            ("TAG".to_string(), "1.2.3".to_string()),
            ("EMPTY".to_string(), String::new()),
            ("OTHER".to_string(), "replacement".to_string()),
            ("ERROR_MESSAGE".to_string(), "must be set".to_string()),
        ]);

        let interpolated = interpolate(
            concat!(
                "nested_default: ${NESTED_TAG:-${TAG:-fallback}}\n",
                "nested_alternative: ${EMPTY:+${OTHER}}\n",
                "nested_unset_alternative: ${UNSET+${OTHER}}\n",
            ),
            &values,
        )
        .expect("compose interpolation should succeed");

        assert_eq!(
            interpolated,
            concat!(
                "nested_default: 1.2.3\n",
                "nested_alternative: \n",
                "nested_unset_alternative: \n",
            )
        );

        let error = interpolate("${REQUIRED?${ERROR_MESSAGE}}", &values)
            .expect_err("required nested message should fail");
        assert!(matches!(
            error,
            ComposeError::InvalidInput(message) if message == "compose variable `REQUIRED` is required: must be set"
        ));
    }

    fn nested_interpolation(depth: usize) -> String {
        let mut text = String::new();
        for _ in 0..depth {
            text.push_str("${UNSET:-");
        }
        text.push_str("fallback");
        for _ in 0..depth {
            text.push('}');
        }
        text
    }

    #[test]
    fn interpolation_allows_depth_below_limit() {
        let interpolated = interpolate(&nested_interpolation(31), &HashMap::new())
            .expect("nesting below the interpolation limit should succeed");

        assert_eq!(interpolated, "fallback");
    }

    #[test]
    fn interpolation_rejects_excessive_nesting() {
        let error = interpolate(&nested_interpolation(32), &HashMap::new())
            .expect_err("excessive nested interpolation should fail");

        assert!(matches!(
            error,
            ComposeError::InvalidInput(message)
                if message == "compose variable interpolation is nested too deeply"
        ));
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
    fn interpolation_does_not_apply_to_mapping_keys() {
        let dir = tempdir().expect("tempdir should be created");
        fs::write(dir.path().join(".env"), "REGISTRY=example.com\nTAG=1.2.3\n")
            .expect("env should be written");
        fs::write(
            dir.path().join("compose.yml"),
            r#"
services:
  app:
    image: ${REGISTRY}/app:${TAG}
    labels:
      "$REGISTRY.key": should_not_interpolate_key
      interpolated.value: "$REGISTRY/value"
"#,
        )
        .expect("compose should be written");

        let resolved = resolve_images(&[], dir.path()).expect("compose file should resolve");

        assert_eq!(resolved.images, vec!["example.com/app:1.2.3".to_string()]);
    }

    #[test]
    fn repeated_compose_files_include_extends_and_build_only_services() {
        let dir = tempdir().expect("tempdir should be created");
        fs::create_dir_all(dir.path().join("base")).expect("base dir should be created");
        fs::create_dir_all(dir.path().join("include")).expect("include dir should be created");
        fs::write(
            dir.path().join(".env"),
            "REGISTRY=registry.example.com\nTAG=1.2.3\nEMPTY=\nFROM_ENV_FILE=dotenv\n",
        )
        .expect("env should be written");
        fs::write(
            dir.path().join("base/base.yml"),
            r#"
services:
  nested:
    image: ${REGISTRY}/nested:${NESTED_TAG:-${TAG:-fallback}}
"#,
        )
        .expect("base compose should be written");
        fs::write(
            dir.path().join("include/included.yml"),
            r#"
services:
  included:
    image: ${REGISTRY}/included:${TAG}
  included_build:
    build: .
"#,
        )
        .expect("included compose should be written");
        let base = dir.path().join("compose.yml");
        let override_file = dir.path().join("compose.override.yml");
        fs::write(
            &base,
            r#"
include:
  - path: include/included.yml

services:
  child:
    extends:
      file: base/base.yml
      service: nested
  override_me:
    image: ${REGISTRY}/old:${TAG}
  build_only:
    build: .
  both:
    image: ${REGISTRY}/both:${FROM_ENV_FILE}
    build: .
"#,
        )
        .expect("root compose should be written");
        fs::write(
            &override_file,
            r#"
services:
  override_me:
    image: ${REGISTRY}/new:${EMPTY:-override-default}
"#,
        )
        .expect("override compose should be written");

        let resolved = resolve_images(&[base, override_file], dir.path())
            .expect("compose files should resolve");

        assert_eq!(
            resolved.images,
            vec![
                "registry.example.com/nested:1.2.3".to_string(),
                "registry.example.com/new:override-default".to_string(),
                "registry.example.com/both:dotenv".to_string(),
                "registry.example.com/included:1.2.3".to_string(),
            ]
        );
        assert_eq!(
            resolved.skipped_build_only,
            vec!["build_only".to_string(), "included_build".to_string()]
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
    fn compose_profiles_match_reference_activation_behavior() {
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
  debug:
    profiles:
      - debug
      - tools
    image: example/debug:latest
"#,
        )
        .expect("compose should be written");

        let resolved = resolve_images(&[], dir.path()).expect("compose file should resolve");
        assert_eq!(resolved.images, vec!["example/default:latest".to_string()]);

        let resolved = resolve_images_with_profiles(&[], dir.path(), &["tools".to_string()], &[])
            .expect("compose file should resolve with a profile");

        assert_eq!(
            resolved.images,
            vec![
                "example/default:latest".to_string(),
                "example/optional:latest".to_string(),
                "example/debug:latest".to_string(),
            ]
        );

        let resolved = resolve_images_with_profiles(&[], dir.path(), &["*".to_string()], &[])
            .expect("compose file should resolve with all profiles");
        assert_eq!(resolved.services.len(), 3);
    }

    #[test]
    fn explicitly_selected_service_activates_its_profiles() {
        let dir = tempdir().expect("tempdir should be created");
        fs::write(
            dir.path().join("compose.yml"),
            r#"
services:
  default:
    image: example/default:latest
  optional:
    profiles: [tools]
    image: example/optional:latest
"#,
        )
        .expect("compose should be written");

        let resolved =
            resolve_images_with_profiles(&[], dir.path(), &[], &["optional".to_string()])
                .expect("selected compose service should resolve");
        let selected =
            select_services(&resolved, &["optional".to_string()]).expect("service should select");

        assert_eq!(selected.images, vec!["example/optional:latest".to_string()]);
    }

    #[test]
    fn compose_profiles_fall_back_to_dotenv() {
        let dir = tempdir().expect("tempdir should be created");
        fs::write(dir.path().join(".env"), "COMPOSE_PROFILES=tools, debug\n")
            .expect("env should be written");
        fs::write(
            dir.path().join("compose.yml"),
            r#"
services:
  default:
    image: example/default:latest
  tools:
    profiles: [tools]
    image: example/tools:latest
  debug:
    profiles: [debug]
    image: example/debug:latest
"#,
        )
        .expect("compose should be written");

        let resolved = resolve_images(&[], dir.path()).expect("compose file should resolve");

        assert_eq!(
            resolved.images,
            vec![
                "example/default:latest".to_string(),
                "example/tools:latest".to_string(),
                "example/debug:latest".to_string(),
            ]
        );

        let resolved = resolve_images_with_profiles(&[], dir.path(), &["other".to_string()], &[])
            .expect("explicit profiles should override the environment");
        assert_eq!(resolved.images, vec!["example/default:latest".to_string()]);
    }
}
