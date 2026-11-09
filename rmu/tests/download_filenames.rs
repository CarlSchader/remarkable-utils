//! Download regressions using synthetic xochitl files and a local SSH stand-in.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::json;
use test_support::Scratch;

const FIRST: &str = "00000000-0000-4000-8000-000000000001";
const SECOND: &str = "00000000-0000-4000-8000-000000000002";

struct Fixture(Scratch);

impl Fixture {
    fn new() -> Self {
        let fixture = Self(Scratch::new("download-names"));
        for dir in ["device", "out", "state"] {
            fs::create_dir(fixture.path(dir)).unwrap();
        }
        fixture.0.mock_command(
            "ssh",
            r#"for arg do command=$arg; done
case "$command" in
    cat\ *|*'tar -cf -'*)
        printf 'transfer\n' >> "$MOCK_TRANSFERS"
        if [ "$MOCK_FAIL_TRANSFERS" = 1 ]; then exit 99; fi
        ;;
esac
exec sh -c "$command"
"#,
        );
        fixture
    }

    fn path(&self, path: &str) -> PathBuf {
        self.0.join(path)
    }

    fn document(&self, uuid: &str, name: &str, file_type: &str) {
        fs::write(
            self.path(&format!("device/{uuid}.metadata")),
            json!({"type": "DocumentType", "visibleName": name, "parent": ""}).to_string(),
        )
        .unwrap();
        fs::write(
            self.path(&format!("device/{uuid}.content")),
            json!({"fileType": file_type}).to_string(),
        )
        .unwrap();
        if matches!(file_type, "pdf" | "epub") {
            fs::write(self.path(&format!("device/{uuid}.{file_type}")), b"payload").unwrap();
        }
    }

    fn download(&self, target: &str, output: Option<&Path>, bundle: bool, fail: bool) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rmu"));
        cmd.current_dir(self.path("out"))
            .env("PATH", self.0.mock_path())
            .env("XDG_STATE_HOME", self.path("state"))
            .env("MOCK_TRANSFERS", self.path("transfers"))
            .env("MOCK_FAIL_TRANSFERS", if fail { "1" } else { "0" })
            .args([
                "--host",
                "mock.invalid",
                "--no-multiplex",
                "--quiet",
                "--xochitl-dir",
            ])
            .arg(self.path("device"))
            .args(["download", target]);
        if let Some(output) = output {
            cmd.arg(output);
        }
        if bundle {
            cmd.arg("--bundle");
        }
        cmd.output().unwrap()
    }

    fn assert_rejected_without_transfer(&self, output: Output) {
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("invalid name:"),
            "unexpected error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !self.path("transfers").exists(),
            "started a document transfer"
        );
    }
}

#[test]
fn traversal_downloads_never_touch_sibling_backups_or_start_transfers() {
    for (file_type, bundle, ext) in [
        ("pdf", false, "pdf"),
        ("notebook", false, "rmdoc"),
        ("", false, "rmdoc"),
        ("pdf", true, "rmdoc"),
    ] {
        for absolute in [false, true] {
            for explicit_directory in [false, true] {
                let fixture = Fixture::new();
                let victim = fixture.path(&format!("victim.{ext}"));
                fs::write(&victim, b"existing backup").unwrap();
                let name = if absolute {
                    fixture.path("victim").to_str().unwrap().to_string()
                } else {
                    "../victim".to_string()
                };
                fixture.document(FIRST, &name, file_type);
                let directory = fixture.path("out");
                // Repeat against the warm listing cache as well. Injecting a
                // transfer failure must not mask the filename validation error.
                for fail in [false, true] {
                    fixture.assert_rejected_without_transfer(fixture.download(
                        FIRST,
                        explicit_directory.then_some(directory.as_path()),
                        bundle,
                        fail,
                    ));
                    assert_eq!(fs::read(&victim).unwrap(), b"existing backup");
                    assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
                }
            }
        }
    }
}

#[test]
fn glob_prevalidates_later_names_and_extensions_before_first_transfer() {
    for (name, file_type, bundle) in [
        ("Z/../../victim", "pdf", false),
        ("Z/../../victim", "notebook", false),
        ("Z/../../victim", "pdf", true),
        ("Z unsafe extension", "pdf/../../victim", false),
    ] {
        let fixture = Fixture::new();
        // Glob traversal sorts A before Z: checking only at transfer time
        // would write this valid document before discovering the unsafe one.
        fixture.document(FIRST, "A safe", "pdf");
        fixture.document(SECOND, name, file_type);
        for fail in [false, true] {
            fixture.assert_rejected_without_transfer(fixture.download(
                "*",
                Some(&fixture.path("out")),
                bundle,
                fail,
            ));
            assert_eq!(fs::read_dir(fixture.path("out")).unwrap().count(), 0);
        }
    }
}

#[test]
fn glob_collisions_still_reject_before_any_transfer() {
    for (second_type, bundle) in [("pdf", false), ("notebook", true)] {
        let fixture = Fixture::new();
        fixture.document(FIRST, "Same name", "pdf");
        fixture.document(SECOND, "Same name", second_type);
        let result = fixture.download("*", Some(&fixture.path("out")), bundle, false);
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("already exists"),
            "unexpected error: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!fixture.path("transfers").exists());
        assert_eq!(fs::read_dir(fixture.path("out")).unwrap().count(), 0);
    }
}

#[test]
fn normal_downloads_and_explicit_user_paths_still_work() {
    for (file_type, bundle, ext) in [
        ("pdf", false, "pdf"),
        ("epub", false, "epub"),
        ("notebook", false, "rmdoc"),
        ("pdf", true, "rmdoc"),
    ] {
        for explicit in [false, true] {
            let fixture = Fixture::new();
            let name = if explicit {
                "../unsafe"
            } else {
                "Lecture 1 日本語"
            };
            fixture.document(FIRST, name, file_type);
            let chosen = PathBuf::from(format!("../chosen.{ext}"));
            let result =
                fixture.download(FIRST, explicit.then_some(chosen.as_path()), bundle, false);
            test_support::assert_success(&result);
            let path = if explicit {
                fixture.path(&format!("chosen.{ext}"))
            } else {
                fixture.path(&format!("out/{name}.{ext}"))
            };
            let bytes = fs::read(path).unwrap();
            if ext == "rmdoc" {
                assert!(bytes.starts_with(b"PK"), "expected a zip bundle");
            } else {
                assert_eq!(bytes, b"payload");
            }
        }
    }
}
