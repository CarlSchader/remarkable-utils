//! A new folder or document must never become visible to xochitl half
//! written: its `.metadata` (what makes xochitl see it) is renamed into
//! place after everything else. The mock `ssh` runs every remote command
//! locally, and its `mv` kills the remote shell (a cut connection: no
//! traps run) after the k-th rename; for every k short of a complete run,
//! no `.metadata` may be left. A bundle whose stream is cut short (its
//! tail, the `.metadata`, lost) must leave nothing visible either. Each
//! scenario runs in its own test process so the mock PATH never races
//! other tests. With `RMU_TEST_BUSYBOX` set (the dev shell, `nix flake
//! check`), every scenario also runs with BusyBox `ash` and applets
//! alone, as on a tablet. Every name is fabricated.
#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use libremarkable_utils::bundle;
use libremarkable_utils::client::Client;
use libremarkable_utils::ssh::{SshOptions, SshSession};
use test_support::Scratch;

/// Runs the remote command (or the `sh -s` script on stdin) locally, with
/// `$MOCK_TOOLS` as the remote's `PATH` if set, and `mv` counting renames
/// in `$RMU_TEST_ROOT/moves` and killing the shell once `$RMU_TEST_CUT`
/// of them are done.
const MOCK_SSH: &str = r#"for cmd do :; done
remote_path=${MOCK_TOOLS:-$PATH}
prelude='mv() {
    if [ -n "$RMU_TEST_CUT" ] && [ "$(cat "$RMU_TEST_ROOT/moves" 2>/dev/null | wc -l)" -ge "$RMU_TEST_CUT" ]; then
        kill -KILL $$
    fi
    command mv "$@" || return
    echo moved >> "$RMU_TEST_ROOT/moves"
}
'
if [ "$cmd" = "sh -s" ]; then
    { printf '%s' "$prelude"; cat; } | PATH=$remote_path sh -s
elif [ -n "$RMU_TEST_TRUNCATE" ]; then
    # Deliver all but the stream's last bytes, then a clean EOF.
    head -c "-$RMU_TEST_TRUNCATE" | PATH=$remote_path sh -c "$prelude$cmd"
else
    PATH=$remote_path sh -c "$prelude$cmd"
fi
"#;

const SCENARIOS: &[&str] = &["folder", "document", "bundle"];

/// BusyBox applets the remote scripts use (the mock's `mv` included).
const APPLETS: &[&str] = &[
    "sh",
    "cat",
    "chmod",
    "chown",
    "kill",
    "mkdir",
    "mktemp",
    "mv",
    "printf",
    "rm",
    "sha256sum",
    "stat",
    "tar",
    "wc",
];

#[test]
fn a_cut_never_leaves_a_metadata_file() {
    let sandbox = Scratch::new("atomic-create");
    sandbox.mock_command("ssh", MOCK_SSH);
    // The host's tools, then BusyBox's alone when available.
    let busybox = std::env::var_os("RMU_TEST_BUSYBOX").map(|busybox| {
        let tools = sandbox.join("busybox");
        fs::create_dir(&tools).unwrap();
        APPLETS.iter().for_each(|applet| {
            std::os::unix::fs::symlink(&busybox, tools.join(applet)).unwrap();
        });
        tools
    });
    std::iter::once(None)
        .chain(busybox.as_deref().map(Some))
        .for_each(|tools| {
            SCENARIOS.iter().for_each(|scenario| {
                // A complete run counts the renames; then a cut after each.
                let total = run(&sandbox, tools, scenario, None, None);
                assert!(total >= 3, "{scenario}: {total} rename(s)");
                (0..total).for_each(|cut| {
                    run(&sandbox, tools, scenario, Some(cut), None);
                });
            });
            // Short by the end-of-archive blocks and part of the `.metadata`.
            run(&sandbox, tools, "bundle", None, Some(1100));
        });
}

/// Run `scenario` in a child process with the remote's `tools` (`None`:
/// the host's), cut after `cut` renames, or its input stream short by
/// `truncate` bytes; the number of renames done.
fn run(
    sandbox: &Scratch,
    tools: Option<&Path>,
    scenario: &str,
    cut: Option<usize>,
    truncate: Option<usize>,
) -> usize {
    let root = sandbox.join(format!(
        "{scenario}-{cut:?}-{truncate:?}-{} 'quoted $dir",
        tools.is_some()
    ));
    fs::create_dir(&root).unwrap();
    let text = |n: Option<usize>| n.map(|n| n.to_string()).unwrap_or_default();
    let mut command = test_support::rerun_test("atomic_item_creation_child");
    command
        .env("PATH", sandbox.mock_path())
        .env("RMU_TEST_ROOT", &root)
        .env("RMU_TEST_SCENARIO", scenario)
        .env("RMU_TEST_CUT", text(cut))
        .env("RMU_TEST_TRUNCATE", text(truncate))
        .env_remove("MOCK_TOOLS");
    if let Some(tools) = tools {
        command.env("MOCK_TOOLS", tools);
    }
    test_support::assert_success(&command.output().unwrap());
    fs::read_to_string(root.join("moves"))
        .unwrap_or_default()
        .lines()
        .count()
}

fn session() -> SshSession {
    SshSession::new(SshOptions {
        destination: "mock-only".to_string(),
        multiplex: false,
        ..SshOptions::default()
    })
    .unwrap()
}

/// A fabricated notebook bundle with one page.
fn rmdoc() -> bundle::Rmdoc {
    let uuid = "22222222-2222-4222-8222-222222222222";
    let mut builder = tar::Builder::new(Vec::new());
    let mut add = |path: &str, data: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, data).unwrap();
    };
    add(
        &format!("{uuid}.metadata"),
        br#"{"type":"DocumentType","visibleName":"Fabricated","parent":"","unknown":1}"#,
    );
    add(&format!("{uuid}.content"), br#"{"fileType":"notebook"}"#);
    add(&format!("{uuid}/page-one.rm"), &[7; 2000]);
    let tar = builder.into_inner().unwrap();
    bundle::parse_rmdoc(&bundle::tar_to_rmdoc(&tar).unwrap()).unwrap()
}

/// The data dir's entries, without the staging leftovers of a cut.
fn visible(data: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(data)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with(".rmu-"))
        .collect();
    names.sort();
    names
}

#[test]
fn atomic_item_creation_child() {
    let Some(root) = std::env::var_os("RMU_TEST_ROOT").map(PathBuf::from) else {
        return;
    };
    let scenario = std::env::var("RMU_TEST_SCENARIO").unwrap();
    let cut = !std::env::var("RMU_TEST_CUT").unwrap().is_empty()
        || !std::env::var("RMU_TEST_TRUNCATE").unwrap().is_empty();
    let data = root.join("data");
    fs::create_dir(&data).unwrap();
    let client = Client::new(session(), data.to_str().unwrap());
    let result = match scenario.as_str() {
        "folder" => client.create_folder_in("Fabricated folder", "").map(drop),
        "document" => client
            .store_payload_bytes(b"%PDF-1.4 fabricated\n", "", "Fabricated", "pdf")
            .map(drop),
        "bundle" => client
            .restore_bundle(rmdoc(), "", "Fabricated restore")
            .map(drop),
        other => panic!("unknown scenario {other}"),
    };
    let names = visible(&data);
    let metadata = names
        .iter()
        .filter(|name| name.ends_with(".metadata"))
        .count();
    let mut log = std::io::stderr();
    writeln!(log, "{scenario} (cut: {cut}): {result:?}, {names:?}").unwrap();
    if cut {
        assert!(result.is_err(), "{scenario}: a cut run must fail");
        assert_eq!(
            metadata, 0,
            "{scenario}: half-created item visible: {names:?}"
        );
        return;
    }
    result.unwrap();
    assert_eq!(metadata, 1, "{names:?}");
    // Everything the item needs is in place, and no staging is left.
    let expected: &[&str] = match scenario.as_str() {
        "folder" => &[".content", ".metadata", ".pagedata"],
        "document" => &["", ".content", ".metadata", ".pagedata", ".pdf"],
        _ => &["", ".content", ".metadata"],
    };
    assert_eq!(names.len(), expected.len(), "{names:?}");
    let uuid = names[0].split('.').next().unwrap();
    expected.iter().for_each(|suffix| {
        assert!(
            data.join(format!("{uuid}{suffix}")).exists(),
            "{suffix}: {names:?}"
        );
    });
    assert_eq!(fs::read_dir(&data).unwrap().count(), expected.len());
    match scenario.as_str() {
        "folder" => {
            assert_eq!(
                fs::read_to_string(data.join(format!("{uuid}.content"))).unwrap(),
                "[]"
            );
            let metadata = fs::read_to_string(data.join(format!("{uuid}.metadata"))).unwrap();
            assert!(metadata.contains("Fabricated folder"), "{metadata}");
        }
        "document" => assert!(data.join(uuid).is_dir()),
        _ => assert_eq!(
            fs::read(data.join(uuid).join("page-one.rm")).unwrap(),
            [7; 2000]
        ),
    }
}
