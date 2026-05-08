use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

use serde_yml::{Mapping, Value};

use crate::error::{DockerPullError, Result};

const DEFAULT_COMPOSE_FILES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

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

#[derive(Debug, Clone)]
struct Service {
    name: String,
    source_file: PathBuf,
    value: Value,
}

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
struct ServiceKey {
    file: PathBuf,
    source_file: PathBuf,
    name: String,
}

pub fn resolve_images(files: &[PathBuf], working_dir: &Path) -> Result<ComposeImages> {
    let entry_files = if files.is_empty() {
        vec![find_default_compose_file(working_dir)?]
    } else {
        files
            .iter()
            .map(|file| absolutize(working_dir, file))
            .collect::<Vec<_>>()
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
        if let Some(image) = mapping_get_string(value_mapping(&resolved), "image") {
            images.push(image.clone());
            service_images.push(ComposeServiceImage {
                service: service.name,
                image: Some(image),
                build_only: false,
            });
        } else if mapping_has_key(value_mapping(&resolved), "build") {
            skipped_build_only.push(service.name.clone());
            service_images.push(ComposeServiceImage {
                service: service.name,
                image: None,
                build_only: true,
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
            DockerPullError::InvalidInput(format!(
                "failed to read compose file `{}`: {error}",
                file.display()
            ))
        })?;
        let text = interpolate(&text, &self.env)?;
        let value: Value = serde_yml::from_str(&text).map_err(|error| {
            DockerPullError::InvalidInput(format!(
                "failed to parse compose file `{}`: {error}",
                file.display()
            ))
        })?;

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
            DockerPullError::InvalidInput(format!(
                "compose file `{}` was not loaded",
                file.display()
            ))
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
                DockerPullError::InvalidInput(format!(
                    "compose service `{}` not found in `{}`",
                    key.name,
                    key.file.display()
                ))
            })
    }

    fn resolve_service(&self, key: &ServiceKey, stack: &mut Vec<ServiceKey>) -> Result<Value> {
        if stack.contains(key) {
            return Err(DockerPullError::InvalidInput(format!(
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

fn find_default_compose_file(working_dir: &Path) -> Result<PathBuf> {
    for name in DEFAULT_COMPOSE_FILES {
        let candidate = working_dir.join(name);
        if candidate.is_file() {
            return normalize_path(&candidate);
        }
    }
    Err(DockerPullError::InvalidInput(format!(
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
        DockerPullError::InvalidInput(format!(
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
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            values.insert(key.trim().to_string(), unquote_env_value(value.trim()));
        }
    }
    for (key, value) in env::vars() {
        values.insert(key, value);
    }
    Ok(values)
}

fn unquote_env_value(value: &str) -> String {
    let quoted = (value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\''));
    if quoted && value.len() >= 2 {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn interpolate(text: &str, values: &HashMap<String, String>) -> Result<String> {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch != '$' {
            output.push(ch);
            continue;
        }
        let Some((_, next)) = chars.peek().copied() else {
            output.push('$');
            continue;
        };
        if next == '$' {
            chars.next();
            output.push('$');
            continue;
        }
        if next != '{' {
            output.push('$');
            continue;
        }
        chars.next();
        let mut expr = String::new();
        let mut closed = false;
        for (_, expr_ch) in chars.by_ref() {
            if expr_ch == '}' {
                closed = true;
                break;
            }
            expr.push(expr_ch);
        }
        if !closed {
            return Err(DockerPullError::InvalidInput(
                "unterminated compose variable interpolation".into(),
            ));
        }
        output.push_str(&resolve_variable(&expr, values)?);
    }
    Ok(output)
}

fn resolve_variable(expr: &str, values: &HashMap<String, String>) -> Result<String> {
    let operators = [":-", "-", ":?", "?", ":+", "+"];
    for operator in operators {
        if let Some((name, extra)) = expr.split_once(operator) {
            return resolve_variable_with_operator(name, operator, extra, values);
        }
    }
    Ok(values.get(expr).cloned().unwrap_or_default())
}

fn resolve_variable_with_operator(
    name: &str,
    operator: &str,
    extra: &str,
    values: &HashMap<String, String>,
) -> Result<String> {
    let value = values.get(name);
    let set = value.is_some();
    let non_empty = value.is_some_and(|value| !value.is_empty());
    match operator {
        ":-" if !non_empty => Ok(extra.to_string()),
        "-" if !set => Ok(extra.to_string()),
        ":?" if !non_empty => Err(DockerPullError::InvalidInput(format!(
            "compose variable `{name}` is required: {extra}"
        ))),
        "?" if !set => Err(DockerPullError::InvalidInput(format!(
            "compose variable `{name}` is required: {extra}"
        ))),
        ":+" if non_empty => Ok(extra.to_string()),
        "+" if set => Ok(extra.to_string()),
        _ => Ok(value.cloned().unwrap_or_default()),
    }
}

fn collect_services(value: &Value, source_file: &Path) -> Vec<Service> {
    let mut services = Vec::new();
    let Some(mapping) = value_mapping(value) else {
        return services;
    };
    let Some(services_value) = mapping_get(Some(mapping), "services") else {
        return services;
    };
    let Some(service_mapping) = value_mapping(services_value) else {
        return services;
    };
    for (name, value) in service_mapping {
        if let Some(name) = name.as_str() {
            services.push(Service {
                name: name.to_string(),
                source_file: source_file.to_path_buf(),
                value: strip_reset_tags(value.clone()),
            });
        }
    }
    services
}

fn collect_includes(value: &Value) -> Vec<PathBuf> {
    let Some(mapping) = value_mapping(value) else {
        return Vec::new();
    };
    let Some(include) = mapping_get(Some(mapping), "include") else {
        return Vec::new();
    };
    match include {
        Value::String(path) => vec![PathBuf::from(path)],
        Value::Sequence(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(path) => Some(PathBuf::from(path)),
                Value::Mapping(mapping) => mapping_get(Some(mapping), "path")
                    .and_then(Value::as_str)
                    .map(PathBuf::from),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn collect_extends_files(value: &Value) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for service in collect_services(value, Path::new("")) {
        let Some(extends) = mapping_get(value_mapping(&service.value), "extends") else {
            continue;
        };
        let Some(file) = mapping_get_string(value_mapping(extends), "file") else {
            continue;
        };
        files.push(PathBuf::from(file));
    }
    files
}

fn strip_reset_tags(value: Value) -> Value {
    match value {
        Value::Tagged(tagged) if tagged.tag == "!reset" => Value::Null,
        Value::Tagged(tagged) => strip_reset_tags(tagged.value),
        Value::Sequence(values) => {
            Value::Sequence(values.into_iter().map(strip_reset_tags).collect())
        }
        Value::Mapping(mapping) => Value::Mapping(
            mapping
                .into_iter()
                .map(|(key, value)| (key, strip_reset_tags(value)))
                .collect(),
        ),
        value => value,
    }
}

fn merge_values(base: Value, override_value: Value) -> Value {
    match (base, override_value) {
        (Value::Mapping(mut base), Value::Mapping(override_map)) => {
            for (key, value) in override_map {
                if key.as_str() == Some("extends") {
                    continue;
                }
                if value == Value::Null {
                    base.remove(&key);
                    continue;
                }
                let merged = match base.remove(&key) {
                    Some(base_value) => merge_values(base_value, value),
                    None => value,
                };
                base.insert(key, merged);
            }
            Value::Mapping(base)
        }
        (_, override_value) => override_value,
    }
}

fn value_mapping(value: &Value) -> Option<&Mapping> {
    match value {
        Value::Mapping(mapping) => Some(mapping),
        _ => None,
    }
}

fn mapping_get<'a>(mapping: Option<&'a Mapping>, key: &str) -> Option<&'a Value> {
    let mapping = mapping?;
    if let Some(value) = mapping.get(Value::String(key.to_string())) {
        return Some(value);
    }
    let merge = mapping.get(Value::String("<<".to_string()))?;
    match merge {
        Value::Mapping(merged) => mapping_get(Some(merged), key),
        Value::Sequence(items) => items
            .iter()
            .filter_map(value_mapping)
            .find_map(|merged| mapping_get(Some(merged), key)),
        _ => None,
    }
}

fn mapping_get_string(mapping: Option<&Mapping>, key: &str) -> Option<String> {
    mapping_get(mapping, key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn mapping_has_key(mapping: Option<&Mapping>, key: &str) -> bool {
    mapping.is_some_and(|mapping| mapping.contains_key(Value::String(key.to_string())))
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
        return Err(DockerPullError::InvalidInput(format!(
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
    use std::fs;

    use tempfile::tempdir;

    use super::{resolve_images, select_services, unique_images};

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
}
