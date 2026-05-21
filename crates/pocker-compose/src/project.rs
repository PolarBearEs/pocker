use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

use serde_yml::Value;

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
        vec![find_default_compose_file(working_dir)?]
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
            ComposeError::InvalidInput(format!(
                "failed to read compose file `{}`: {error}",
                file.display()
            ))
        })?;
        let text = interpolate(&text, &self.env)?;
        let value: Value = serde_yml::from_str(&text).map_err(|error| {
            ComposeError::InvalidInput(format!(
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

fn parse_env_file(text: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    let mut lines = text.lines();

    while let Some(line) = lines.next() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = line
            .strip_prefix("export ")
            .map(str::trim_start)
            .unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }

        values.insert(
            key.to_string(),
            parse_env_value(value.trim_start(), &mut lines),
        );
    }

    values
}

fn parse_env_value<'a>(value: &str, lines: &mut impl Iterator<Item = &'a str>) -> String {
    if let Some(rest) = value.strip_prefix('"') {
        parse_quoted_env_value(rest, '"', lines)
    } else if let Some(rest) = value.strip_prefix('\'') {
        parse_quoted_env_value(rest, '\'', lines)
    } else {
        parse_unquoted_env_value(value)
    }
}

fn parse_quoted_env_value<'a>(
    first: &str,
    quote: char,
    lines: &mut impl Iterator<Item = &'a str>,
) -> String {
    let mut value = String::new();
    let mut current = first;
    let mut escaped = false;

    loop {
        for ch in current.chars() {
            if escaped {
                value.push(match ch {
                    'n' if quote == '"' => '\n',
                    'r' if quote == '"' => '\r',
                    't' if quote == '"' => '\t',
                    other => other,
                });
                escaped = false;
                continue;
            }

            if quote == '"' && ch == '\\' {
                escaped = true;
                continue;
            }

            if ch == quote {
                return value;
            }

            value.push(ch);
        }

        let Some(next) = lines.next() else {
            return value;
        };
        value.push('\n');
        current = next;
    }
}

fn parse_unquoted_env_value(value: &str) -> String {
    let mut output = String::new();
    let mut escaped = false;
    let mut previous_was_whitespace = false;

    for ch in value.chars() {
        if escaped {
            output.push(ch);
            previous_was_whitespace = ch.is_whitespace();
            escaped = false;
            continue;
        }

        if ch == '\\' {
            escaped = true;
            continue;
        }

        if ch == '#' && previous_was_whitespace {
            break;
        }

        output.push(ch);
        previous_was_whitespace = ch.is_whitespace();
    }

    output.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::parse_env_file;

    #[test]
    fn env_file_parser_handles_compose_dotenv_forms() {
        let values = parse_env_file(concat!(
            "# comment\n",
            "export REGISTRY=example.com # registry host\n",
            "TAG=\"1.2.3\"\n",
            "LITERAL_HASH=#literal\n",
            "SUFFIX=foo\\#bar # comment\n",
            "HASHED='pa#ss'\n",
            "QUOTED=\"hello \\\"there\\\"\"\n",
            "MULTILINE=\"line1\n",
            "line2\"\n",
        ));

        assert_eq!(
            values.get("REGISTRY").map(String::as_str),
            Some("example.com")
        );
        assert_eq!(values.get("TAG").map(String::as_str), Some("1.2.3"));
        assert_eq!(
            values.get("LITERAL_HASH").map(String::as_str),
            Some("#literal")
        );
        assert_eq!(values.get("SUFFIX").map(String::as_str), Some("foo#bar"));
        assert_eq!(values.get("HASHED").map(String::as_str), Some("pa#ss"));
        assert_eq!(
            values.get("QUOTED").map(String::as_str),
            Some("hello \"there\"")
        );
        assert_eq!(
            values.get("MULTILINE").map(String::as_str),
            Some("line1\nline2")
        );
    }
}
