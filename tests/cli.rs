use std::fs;

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

fn pocker() -> Command {
    Command::cargo_bin("pocker").expect("pocker binary should be built")
}

#[test]
fn version_subcommand_prints_name_and_version() {
    pocker()
        .arg("version")
        .assert()
        .success()
        .stdout(contains(env!("CARGO_PKG_NAME")))
        .stdout(contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn version_flags_print_name_and_version() {
    for flag in ["--version", "-V"] {
        pocker()
            .arg(flag)
            .assert()
            .success()
            .stdout(contains(env!("CARGO_PKG_NAME")))
            .stdout(contains(env!("CARGO_PKG_VERSION")));
    }
}

#[test]
fn help_flag_exits_success() {
    pocker().arg("--help").assert().success();
}

#[test]
fn cache_clean_summarizes_deleted_files_by_default() {
    let dir = tempfile::tempdir().expect("tempdir should create");
    seed_cache_clean_files(dir.path());

    pocker()
        .arg("--cache-dir")
        .arg(dir.path())
        .arg("cache")
        .arg("clean")
        .assert()
        .success()
        .stdout(contains("Deleted:"))
        .stdout(contains("Cached files/layers: 1 file (4 B)"))
        .stdout(contains("Coordination files: 1 file (4 B)"))
        .stdout(contains("Reclaimed space: 8 B"))
        .stdout(contains("locks/images/stale.lock").not());
}

#[test]
fn cache_clean_verbose_prints_deleted_file_list() {
    let dir = tempfile::tempdir().expect("tempdir should create");
    seed_cache_clean_files(dir.path());
    let blob = std::path::PathBuf::from("blobs")
        .join("sha256")
        .join("4444444444444444444444444444444444444444444444444444444444444444");
    let lock = std::path::PathBuf::from("locks")
        .join("images")
        .join("stale.lock");

    pocker()
        .arg("--cache-dir")
        .arg(dir.path())
        .arg("cache")
        .arg("clean")
        .arg("-v")
        .assert()
        .success()
        .stdout(contains("Deleted:"))
        .stdout(contains(format!("{} (4 B)", blob.display())))
        .stdout(contains(format!("{} (4 B)", lock.display())));
}

fn seed_cache_clean_files(root: &std::path::Path) {
    let blob = root
        .join("blobs")
        .join("sha256")
        .join("4444444444444444444444444444444444444444444444444444444444444444");
    let lock = root.join("locks").join("images").join("stale.lock");
    fs::create_dir_all(blob.parent().expect("blob parent should exist"))
        .expect("blob parent should create");
    fs::create_dir_all(lock.parent().expect("lock parent should exist"))
        .expect("lock parent should create");
    fs::write(blob, b"blob").expect("blob should write");
    fs::write(lock, b"lock").expect("lock should write");
}

#[test]
fn compose_config_images_lists_unique_images_from_file() {
    let dir = tempfile::tempdir().expect("tempdir should create");
    let compose_path = dir.path().join("docker-compose.yml");
    fs::write(
        &compose_path,
        "services:\n  web:\n    image: nginx:alpine\n  api:\n    image: nginx:alpine\n  builder:\n    build: .\n",
    )
    .expect("compose file should write");

    pocker()
        .arg("compose")
        .arg("-f")
        .arg(&compose_path)
        .arg("config")
        .arg("--images")
        .assert()
        .success()
        .stdout("nginx:alpine\n");
}

#[test]
fn compose_config_profiles_match_docker_compose_behavior() {
    let dir = tempfile::tempdir().expect("tempdir should create");
    let compose_path = dir.path().join("docker-compose.yml");
    fs::write(
        &compose_path,
        concat!(
            "services:\n",
            "  default:\n",
            "    image: example/default:latest\n",
            "  tools:\n",
            "    profiles: [tools]\n",
            "    image: example/tools:latest\n",
        ),
    )
    .expect("compose file should write");

    pocker()
        .arg("compose")
        .arg("-f")
        .arg(&compose_path)
        .arg("config")
        .arg("--images")
        .assert()
        .success()
        .stdout("example/default:latest\n");

    pocker()
        .arg("compose")
        .arg("-f")
        .arg(&compose_path)
        .args(["--profile", "tools"])
        .arg("config")
        .arg("--images")
        .assert()
        .success()
        .stdout("example/default:latest\nexample/tools:latest\n");

    pocker()
        .arg("compose")
        .arg("-f")
        .arg(&compose_path)
        .args(["--profile", "other"])
        .arg("config")
        .arg("--images")
        .arg("tools")
        .assert()
        .success()
        .stdout("example/tools:latest\n");
}

#[test]
fn compose_profiles_process_env_overrides_dotenv_and_supports_wildcard() {
    let dir = tempfile::tempdir().expect("tempdir should create");
    fs::write(dir.path().join(".env"), "COMPOSE_PROFILES=missing\n").expect("env should write");
    let compose_path = dir.path().join("docker-compose.yml");
    fs::write(
        &compose_path,
        concat!(
            "services:\n",
            "  default:\n",
            "    image: example/default:latest\n",
            "  tools:\n",
            "    profiles: [tools]\n",
            "    image: example/tools:latest\n",
            "  debug:\n",
            "    profiles: [debug]\n",
            "    image: example/debug:latest\n",
        ),
    )
    .expect("compose file should write");

    pocker()
        .env("COMPOSE_PROFILES", "*")
        .arg("compose")
        .arg("-f")
        .arg(&compose_path)
        .arg("config")
        .arg("--images")
        .assert()
        .success()
        .stdout("example/default:latest\nexample/tools:latest\nexample/debug:latest\n");
}

#[test]
fn compose_config_does_not_interpolate_mapping_keys() {
    let dir = tempfile::tempdir().expect("tempdir should create");
    let compose_path = dir.path().join("docker-compose.yml");
    fs::write(
        &compose_path,
        concat!(
            "services:\n",
            "  web:\n",
            "    image: nginx:alpine\n",
            "    labels:\n",
            "      \"${REQUIRED?label keys should not interpolate}\": literal\n",
        ),
    )
    .expect("compose file should write");

    pocker()
        .arg("compose")
        .arg("-f")
        .arg(&compose_path)
        .arg("config")
        .arg("--images")
        .assert()
        .success()
        .stdout("nginx:alpine\n");
}

#[test]
fn compose_config_json_prints_resolved_model() {
    let dir = tempfile::tempdir().expect("tempdir should create");
    let compose_path = dir.path().join("docker-compose.yml");
    fs::write(
        &compose_path,
        concat!(
            "services:\n",
            "  web:\n",
            "    image: nginx:alpine\n",
            "    labels:\n",
            "      app.example/name: web\n",
            "      enabled: true\n",
            "  api:\n",
            "    image: nginx:alpine\n",
            "    labels:\n",
            "      - role=api\n",
            "      - flag-only\n",
            "  builder:\n",
            "    build: .\n",
        ),
    )
    .expect("compose file should write");

    let output = pocker()
        .arg("compose")
        .arg("-f")
        .arg(&compose_path)
        .arg("config")
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value =
        serde_json::from_slice(&output).expect("config output should be json");

    assert_eq!(value["images"], serde_json::json!(["nginx:alpine"]));
    assert_eq!(value["skipped_build_only"], serde_json::json!(["builder"]));
    assert_eq!(
        value["services"],
        serde_json::json!([
            {
                "name": "web",
                "image": "nginx:alpine",
                "build_only": false,
                "labels": {"app.example/name": "web", "enabled": "true"},
            },
            {
                "name": "api",
                "image": "nginx:alpine",
                "build_only": false,
                "labels": {"role": "api", "flag-only": null},
            },
            {"name": "builder", "image": null, "build_only": true},
        ])
    );
}

#[test]
fn compose_config_pull_plan_requires_explicit_flag() {
    let dir = tempfile::tempdir().expect("tempdir should create");
    let compose_path = dir.path().join("docker-compose.yml");
    fs::write(
        &compose_path,
        "services:\n  web:\n    image: nginx:alpine\n",
    )
    .expect("compose file should write");

    pocker()
        .arg("compose")
        .arg("-f")
        .arg(&compose_path)
        .arg("config")
        .arg("--pull-plan")
        .assert()
        .success()
        .stdout("")
        .stderr(contains("Compose pull plan:"))
        .stderr(contains("web"));
}

#[test]
fn compose_config_output_modes_conflict() {
    pocker()
        .args(["compose", "config", "--images", "--pull-plan"])
        .assert()
        .failure()
        .stderr(contains("cannot be used with"));

    pocker()
        .args(["compose", "config", "--images", "--format", "json"])
        .assert()
        .failure()
        .stderr(contains("cannot be used with"));
}
