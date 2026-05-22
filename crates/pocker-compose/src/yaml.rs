use std::path::{Path, PathBuf};

use serde_yml::{Mapping, Value};

#[derive(Debug, Clone)]
pub(super) struct Service {
    pub(super) name: String,
    pub(super) source_file: PathBuf,
    pub(super) value: Value,
}

pub(super) fn collect_services(value: &Value, source_file: &Path) -> Vec<Service> {
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

pub(super) fn collect_includes(value: &Value) -> Vec<PathBuf> {
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

pub(super) fn collect_extends_files(value: &Value) -> Vec<PathBuf> {
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

pub(super) fn merge_values(base: Value, override_value: Value) -> Value {
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

pub(super) fn value_mapping(value: &Value) -> Option<&Mapping> {
    match value {
        Value::Mapping(mapping) => Some(mapping),
        _ => None,
    }
}

pub(super) fn mapping_get<'a>(mapping: Option<&'a Mapping>, key: &str) -> Option<&'a Value> {
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

pub(super) fn mapping_get_string(mapping: Option<&Mapping>, key: &str) -> Option<String> {
    mapping_get(mapping, key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

pub(super) fn mapping_has_key(mapping: Option<&Mapping>, key: &str) -> bool {
    mapping_get(mapping, key).is_some()
}
