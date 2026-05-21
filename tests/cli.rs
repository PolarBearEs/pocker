use std::fs;

use assert_cmd::Command;
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
fn compose_config_json_prints_resolved_model() {
    let dir = tempfile::tempdir().expect("tempdir should create");
    let compose_path = dir.path().join("docker-compose.yml");
    fs::write(
        &compose_path,
        concat!(
            "services:\n",
            "  web:\n",
            "    image: nginx:alpine\n",
            "  api:\n",
            "    image: nginx:alpine\n",
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
            {"name": "web", "image": "nginx:alpine", "build_only": false},
            {"name": "api", "image": "nginx:alpine", "build_only": false},
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
