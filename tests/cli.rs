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
