use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

use serde_yml::{Mapping, Value};

use crate::env::parse_env_file;
use crate::interpolate::interpolate;
use crate::yaml::{
    Service, collect_extends_files, collect_includes, collect_services, mapping_get,
    mapping_get_string, mapping_has_key, merge_values, value_mapping,
};
use crate::{ComposeError, ComposeImages, ComposeServiceImage, Result};

const DEFAULT_COMPOSE_FILES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];
const DEFAULT_COMPOSE_OVERRIDE_FILES: [&str; 4] = [
    "compose.override.yaml",
    "compose.override.yml",
    "docker-compose.override.yaml",
    "docker-compose.override.yml",
];

#[derive(Debug)]
struct ComposeProject {
    env: HashMap<String, String>,
    documents: HashMap<PathBuf, ComposeDocument>,
    entry_files: Vec<PathBuf>,
    project_dir: PathBuf,
}

#[derive(Debug)]
struct ComposeDocument {
    services: Vec<Service>,
    includes: Vec<PathBuf>,
}

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
struct ServiceKey {
    file: PathBuf,
    source_file: PathBuf,
    name: String,
}

pub fn resolve_images(files: &[PathBuf], working_dir: &Path) -> Result<ComposeImages> {
    let entry_files = if files.is_empty() {
        find_default_compose_files(working_dir)?
    } else {
        files
            .iter()
            .map(|file| absolutize(working_dir, file))
            .map(|file| normalize_path(&file))
            .collect::<Result<Vec<_>>>()?
    };
    let project_dir = entry_files
        .first()
        .and_then(|file| file.parent())
        .unwrap_or(working_dir);
    let env = load_compose_env(project_dir)?;
    let mut project = ComposeProject {
        env,
        documents: HashMap::new(),
        entry_files: entry_files.clone(),
        project_dir: project_dir.to_path_buf(),
    };
    for file in &entry_files {
        project.load_document(file)?;
    }

    let synthetic_file = PathBuf::from("<merged-compose>");
    let mut merged_services = Vec::new();
    for file in project.entry_files.clone() {
        for service in project.document(&file)?.services.clone() {
            if let Some(existing) = merged_services
                .iter_mut()
                .find(|existing: &&mut Service| existing.name == service.name)
            {
                existing.value = merge_values(existing.value.clone(), service.value);
                existing.source_file = service.source_file;
            } else {
                merged_services.push(service);
            }
        }
    }

    let entry_set = project.entry_files.iter().cloned().collect::<HashSet<_>>();
    let mut include_files = Vec::new();
    for file in &project.entry_files {
        project.collect_include_files(file, &mut include_files)?;
    }
    for file in include_files {
        if entry_set.contains(&file) {
            continue;
        }
        for service in project.document(&file)?.services.clone() {
            if !merged_services
                .iter()
                .any(|existing: &Service| existing.name == service.name)
            {
                merged_services.push(service);
            }
        }
    }

    project.documents.insert(
        synthetic_file.clone(),
        ComposeDocument {
            services: merged_services,
            includes: Vec::new(),
        },
    );

    let mut images = Vec::new();
    let mut skipped_build_only = Vec::new();
    let mut service_images = Vec::new();
    let services = project.document(&synthetic_file)?.services.clone();
    for service in services {
        let key = ServiceKey {
            file: synthetic_file.clone(),
            source_file: service.source_file.clone(),
            name: service.name.clone(),
        };
        let resolved = project.resolve_service(&key, &mut Vec::new())?;
        let labels = service_labels(&resolved);
        if let Some(image) = mapping_get_string(value_mapping(&resolved), "image") {
            images.push(image.clone());
            service_images.push(ComposeServiceImage {
                service: service.name,
                image: Some(image),
                build_only: false,
                labels,
            });
        } else if mapping_has_key(value_mapping(&resolved), "build") {
            skipped_build_only.push(service.name.clone());
            service_images.push(ComposeServiceImage {
                service: service.name,
                image: None,
                build_only: true,
                labels,
            });
        }
    }

    Ok(ComposeImages {
        images,
        skipped_build_only,
        services: service_images,
    })
}

impl ComposeProject {
    fn load_document(&mut self, file: &Path) -> Result<()> {
        let file = normalize_path(file)?;
        if self.documents.contains_key(&file) {
            return Ok(());
        }

        let text = std::fs::read_to_string(&file).map_err(|error| {
            ComposeError::InvalidInput(format!(
                "failed to read compose file `{}`: {error}",
                file.display()
            ))
        })?;
        let value: Value = serde_yml::from_str(&text).map_err(|error| {
            ComposeError::InvalidInput(format!(
                "failed to parse compose file `{}`: {error}",
                file.display()
            ))
        })?;
        let value = interpolate_value(value, &self.env)?;

        self.documents.insert(
            file.clone(),
            ComposeDocument {
                services: collect_services(&value, &file),
                includes: collect_includes(&value)
                    .into_iter()
                    .map(|include| {
                        absolutize(file.parent().unwrap_or_else(|| Path::new(".")), &include)
                    })
                    .map(|include| normalize_path(&include))
                    .collect::<Result<Vec<_>>>()?,
            },
        );

        for include in self.document(&file)?.includes.clone() {
            self.load_document(&include)?;
        }
        for extends_file in collect_extends_files(&value) {
            let extends_file = absolutize(
                file.parent().unwrap_or_else(|| Path::new(".")),
                &extends_file,
            );
            self.load_document(&extends_file)?;
        }
        Ok(())
    }

    fn document(&self, file: &Path) -> Result<&ComposeDocument> {
        self.documents.get(file).ok_or_else(|| {
            ComposeError::InvalidInput(format!("compose file `{}` was not loaded", file.display()))
        })
    }

    fn collect_include_files(&self, file: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
        let document = self.document(file)?;
        for include in &document.includes {
            if output.contains(include) {
                continue;
            }
            output.push(include.clone());
            self.collect_include_files(include, output)?;
        }
        Ok(())
    }

    fn service(&self, key: &ServiceKey) -> Result<Service> {
        let document = self.document(&key.file)?;
        document
            .services
            .iter()
            .find(|service| service.name == key.name)
            .cloned()
            .ok_or_else(|| {
                ComposeError::InvalidInput(format!(
                    "compose service `{}` not found in `{}`",
                    key.name,
                    key.file.display()
                ))
            })
    }

    fn resolve_service(&self, key: &ServiceKey, stack: &mut Vec<ServiceKey>) -> Result<Value> {
        if stack.contains(key) {
            return Err(ComposeError::InvalidInput(format!(
                "compose extends cycle at service `{}`",
                key.name
            )));
        }
        stack.push(key.clone());

        let service = self.service(key)?;
        let Some(extends) = mapping_get(value_mapping(&service.value), "extends") else {
            stack.pop();
            return Ok(service.value);
        };
        let Some(parent_key) = self.extends_key(key, extends)? else {
            stack.pop();
            return Ok(service.value);
        };
        let parent = self.resolve_service(&parent_key, stack)?;
        stack.pop();

        Ok(merge_values(parent, service.value))
    }

    fn extends_key(&self, key: &ServiceKey, value: &Value) -> Result<Option<ServiceKey>> {
        if let Some(service) = value.as_str() {
            return Ok(Some(ServiceKey {
                file: key.file.clone(),
                source_file: key.source_file.clone(),
                name: service.to_string(),
            }));
        }
        let mapping = value_mapping(value);
        if mapping.is_none() {
            return Ok(None);
        }
        let mapping = mapping.unwrap();
        let Some(service) = mapping_get_string(Some(mapping), "service") else {
            return Ok(None);
        };
        let file = mapping_get_string(Some(mapping), "file")
            .map(|file| {
                let base = key
                    .source_file
                    .parent()
                    .filter(|_| key.source_file.is_absolute())
                    .unwrap_or(&self.project_dir);
                absolutize(base, Path::new(&file))
            })
            .transpose_normalize()?
            .unwrap_or_else(|| key.file.clone());
        Ok(Some(ServiceKey {
            source_file: file.clone(),
            file,
            name: service,
        }))
    }
}

trait NormalizeOptionPath {
    fn transpose_normalize(self) -> Result<Option<PathBuf>>;
}

impl NormalizeOptionPath for Option<PathBuf> {
    fn transpose_normalize(self) -> Result<Option<PathBuf>> {
        self.map(|path| normalize_path(&path)).transpose()
    }
}

fn find_default_compose_files(working_dir: &Path) -> Result<Vec<PathBuf>> {
    let base_file = find_default_compose_file(working_dir)?;
    let mut files = vec![base_file];
    for name in DEFAULT_COMPOSE_OVERRIDE_FILES {
        let candidate = working_dir.join(name);
        if candidate.is_file() {
            files.push(normalize_path(&candidate)?);
        }
    }
    Ok(files)
}

fn find_default_compose_file(working_dir: &Path) -> Result<PathBuf> {
    for name in DEFAULT_COMPOSE_FILES {
        let candidate = working_dir.join(name);
        if candidate.is_file() {
            return normalize_path(&candidate);
        }
    }
    Err(ComposeError::InvalidInput(format!(
        "no compose file found in `{}`",
        working_dir.display()
    )))
}

fn absolutize(base: &Path, file: &Path) -> PathBuf {
    if file.is_absolute() {
        file.to_path_buf()
    } else {
        base.join(file)
    }
}

fn normalize_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize().map_err(|error| {
        ComposeError::InvalidInput(format!(
            "failed to resolve compose path `{}`: {error}",
            path.display()
        ))
    })
}

fn load_compose_env(project_dir: &Path) -> Result<HashMap<String, String>> {
    let mut values = HashMap::new();
    let env_path = project_dir.join(".env");
    if env_path.is_file() {
        let text = std::fs::read_to_string(&env_path)?;
        values.extend(parse_env_file(&text));
    }
    for (key, value) in env::vars() {
        values.insert(key, value);
    }
    Ok(values)
}

fn service_labels(service: &Value) -> Option<BTreeMap<String, Option<String>>> {
    let labels = mapping_get(value_mapping(service), "labels")?;
    let labels = match labels {
        Value::Mapping(mapping) => mapping
            .iter()
            .filter_map(|(key, value)| {
                let key = key.as_str()?.to_string();
                let value = match value {
                    Value::Null => None,
                    Value::String(value) => Some(value.clone()),
                    Value::Bool(value) => Some(value.to_string()),
                    Value::Number(value) => Some(value.to_string()),
                    _ => return None,
                };
                Some((key, value))
            })
            .collect::<BTreeMap<_, _>>(),
        Value::Sequence(items) => items
            .iter()
            .filter_map(|item| {
                let item = item.as_str()?;
                let (key, value) = item
                    .split_once('=')
                    .map(|(key, value)| (key, Some(value.to_string())))
                    .unwrap_or((item, None));
                Some((key.to_string(), value))
            })
            .collect::<BTreeMap<_, _>>(),
        _ => BTreeMap::new(),
    };

    (!labels.is_empty()).then_some(labels)
}

fn interpolate_value(value: Value, env: &HashMap<String, String>) -> Result<Value> {
    match value {
        Value::String(value) => interpolate(&value, env).map(Value::String),
        Value::Sequence(values) => values
            .into_iter()
            .map(|value| interpolate_value(value, env))
            .collect::<Result<Vec<_>>>()
            .map(Value::Sequence),
        Value::Mapping(mapping) => {
            let mut interpolated = Mapping::new();
            for (key, value) in mapping {
                interpolated.insert(key, interpolate_value(value, env)?);
            }
            Ok(Value::Mapping(interpolated))
        }
        Value::Tagged(tagged) => Ok(Value::Tagged(Box::new(serde_yml::value::TaggedValue {
            tag: tagged.tag,
            value: interpolate_value(tagged.value, env)?,
        }))),
        value => Ok(value),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_yml::Value;

    use super::{interpolate_value, service_labels};
    use crate::yaml::{mapping_get, value_mapping};

    #[test]
    fn interpolate_value_interpolates_values_but_not_mapping_keys() {
        let value = serde_yml::from_str::<Value>(
            r#"
services:
  app:
    image: ${REGISTRY}/app:${TAG}
    labels:
      "$REGISTRY.key": should_not_interpolate_key
      interpolated.value: "$REGISTRY/value"
"#,
        )
        .expect("yaml should parse");
        let env = HashMap::from([
            ("REGISTRY".to_string(), "example.com".to_string()),
            ("TAG".to_string(), "1.2.3".to_string()),
        ]);

        let value = interpolate_value(value, &env).expect("interpolation should succeed");
        let root = value_mapping(&value);
        let services = mapping_get(root, "services").and_then(value_mapping);
        let app = mapping_get(services, "app").and_then(value_mapping);
        let labels = mapping_get(app, "labels").and_then(value_mapping);

        assert_eq!(
            mapping_get(app, "image").and_then(Value::as_str),
            Some("example.com/app:1.2.3")
        );
        assert_eq!(
            mapping_get(labels, "$REGISTRY.key").and_then(Value::as_str),
            Some("should_not_interpolate_key")
        );
        assert_eq!(
            mapping_get(labels, "interpolated.value").and_then(Value::as_str),
            Some("example.com/value")
        );
        assert!(
            mapping_get(labels, "example.com.key").is_none(),
            "mapping keys must not be interpolated"
        );
    }

    #[test]
    fn service_labels_supports_mapping_and_sequence_forms() {
        let mapping_service = serde_yml::from_str::<Value>(
            r#"
image: app
labels:
  app.example/name: web
  enabled: true
  priority: 3
  empty:
"#,
        )
        .expect("mapping labels should parse");
        let mapping_labels = service_labels(&mapping_service).expect("labels should resolve");

        assert_eq!(
            mapping_labels.get("app.example/name"),
            Some(&Some("web".to_string()))
        );
        assert_eq!(
            mapping_labels.get("enabled"),
            Some(&Some("true".to_string()))
        );
        assert_eq!(mapping_labels.get("priority"), Some(&Some("3".to_string())));
        assert_eq!(mapping_labels.get("empty"), Some(&None));

        let sequence_service = serde_yml::from_str::<Value>(
            r#"
image: app
labels:
  - role=api
  - flag-only
"#,
        )
        .expect("sequence labels should parse");
        let sequence_labels = service_labels(&sequence_service).expect("labels should resolve");

        assert_eq!(sequence_labels.get("role"), Some(&Some("api".to_string())));
        assert_eq!(sequence_labels.get("flag-only"), Some(&None));
    }
}
