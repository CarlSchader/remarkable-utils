//! Golden `--json` output, one per report type and error shape. The
//! format is unstable until device validation, but any change (in the
//! CLI or in the library's reports) must show up here: update these
//! deliberately, together with the README.

use super::*;
use crate::hub::json;

/// A sync report.
#[test]
fn sync_report_json() {
    let value = json::report(&Report::Sync(sync_report()));
    assert_eq!(
        value,
        json!({
            "applied": [
                "8c1f0b9e-0000-4000-8000-000000000001",
                "8c1f0b9e-0000-4000-8000-000000000002",
                "8c1f0b9e-0000-4000-8000-000000000003",
                "8c1f0b9e-0000-4000-8000-000000000015",
                "8c1f0b9e-0000-4000-8000-000000000017"
            ],
            "base": "cccccccccccccccccccccccccccccccccccccccc",
            "command": "sync",
            "deletions": {
                "allowed": true,
                "empty_snapshot": false,
                "exempt": [
                    "8c1f0b9e-0000-4000-8000-000000000017"
                ],
                "from_origin": [
                    "8c1f0b9e-0000-4000-8000-000000000014"
                ],
                "from_tablet": [
                    "8c1f0b9e-0000-4000-8000-000000000015",
                    "8c1f0b9e-0000-4000-8000-000000000016"
                ],
                "needs_consent": true
            },
            "forks": [
                {
                    "copy": "8c1f0b9e-0000-4000-8000-00000000000a",
                    "name": "Lecture notes (conflict Desk tablet 2026-11-09)",
                    "original": "8c1f0b9e-0000-4000-8000-000000000009"
                }
            ],
            "inferred": {},
            "names": {
                "8c1f0b9e-0000-4000-8000-000000000014": "Removed on this tablet",
                "8c1f0b9e-0000-4000-8000-000000000015": "Old draft",
                "8c1f0b9e-0000-4000-8000-000000000016": "Second draft",
                "8c1f0b9e-0000-4000-8000-000000000017": "Cleaned up elsewhere"
            },
            "notes": [
                {
                    "deleted_by": "theirs",
                    "doc": "8c1f0b9e-0000-4000-8000-000000000007",
                    "kind": "kept_over_deletion"
                },
                {
                    "doc": "8c1f0b9e-0000-4000-8000-000000000008",
                    "kind": "restored_from_trash",
                    "parent": ""
                }
            ],
            "orphans": [
                "8c1f0b9e-0000-4000-8000-000000000006"
            ],
            "push_attempts": 2,
            "quarantined": {
                "8c1f0b9e-0000-4000-8000-000000000005": "unparsable .metadata"
            },
            "read": {
                "downloaded": {
                    "bytes": 2048,
                    "files": 7
                },
                "unmirrored": 3
            },
            "reconciled": {
                "dropped": false,
                "finished": [
                    "8c1f0b9e-0000-4000-8000-00000000000b"
                ],
                "kept": [
                    "8c1f0b9e-0000-4000-8000-00000000000c"
                ]
            },
            "schema": 1,
            "skipped": [
                "8c1f0b9e-0000-4000-8000-000000000004",
                "8c1f0b9e-0000-4000-8000-000000000016"
            ],
            "snapshot": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "summary": {
                "added": 0,
                "changed_ours": 2,
                "changed_theirs": 3,
                "deleted": 0,
                "forked": 1,
                "identical": 0,
                "kept": 0,
                "merged": 0,
                "resurrected": 0,
                "unchanged": 40
            },
            "took_over": {
                "previous": {
                    "boot_id": "0f0e0d0c-0000-4000-8000-0000000000b1",
                    "host": "laptop",
                    "pid": 4242,
                    "run": "run-1",
                    "started_unix": 1790000000
                },
                "reason": "boot_changed"
            },
            "update": {
                "commit": "cccccccccccccccccccccccccccccccccccccccc",
                "kind": "merge"
            },
            "warnings": [
                {
                    "kind": "mixed_model",
                    "model": "reMarkable Paper Pro",
                    "seen": "reMarkable 2"
                }
            ]
        })
    );
}

/// A clone that pulled and matched a document to origin's history.
#[test]
fn clone_report_json() {
    let value = json::report(&Report::Clone(SyncReport {
        update: MainUpdate::Pulled,
        inferred: BTreeMap::from([(doc(1), oid('a'))]),
        reconciled: None,
        took_over: None,
        warnings: Vec::new(),
        ..sync_report()
    }));
    assert_eq!(
        value,
        json!({
            "applied": [
                "8c1f0b9e-0000-4000-8000-000000000001",
                "8c1f0b9e-0000-4000-8000-000000000002",
                "8c1f0b9e-0000-4000-8000-000000000003",
                "8c1f0b9e-0000-4000-8000-000000000015",
                "8c1f0b9e-0000-4000-8000-000000000017"
            ],
            "base": "cccccccccccccccccccccccccccccccccccccccc",
            "command": "clone",
            "deletions": {
                "allowed": true,
                "empty_snapshot": false,
                "exempt": [
                    "8c1f0b9e-0000-4000-8000-000000000017"
                ],
                "from_origin": [
                    "8c1f0b9e-0000-4000-8000-000000000014"
                ],
                "from_tablet": [
                    "8c1f0b9e-0000-4000-8000-000000000015",
                    "8c1f0b9e-0000-4000-8000-000000000016"
                ],
                "needs_consent": true
            },
            "forks": [
                {
                    "copy": "8c1f0b9e-0000-4000-8000-00000000000a",
                    "name": "Lecture notes (conflict Desk tablet 2026-11-09)",
                    "original": "8c1f0b9e-0000-4000-8000-000000000009"
                }
            ],
            "inferred": {
                "8c1f0b9e-0000-4000-8000-000000000001": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            },
            "names": {
                "8c1f0b9e-0000-4000-8000-000000000014": "Removed on this tablet",
                "8c1f0b9e-0000-4000-8000-000000000015": "Old draft",
                "8c1f0b9e-0000-4000-8000-000000000016": "Second draft",
                "8c1f0b9e-0000-4000-8000-000000000017": "Cleaned up elsewhere"
            },
            "notes": [
                {
                    "deleted_by": "theirs",
                    "doc": "8c1f0b9e-0000-4000-8000-000000000007",
                    "kind": "kept_over_deletion"
                },
                {
                    "doc": "8c1f0b9e-0000-4000-8000-000000000008",
                    "kind": "restored_from_trash",
                    "parent": ""
                }
            ],
            "orphans": [
                "8c1f0b9e-0000-4000-8000-000000000006"
            ],
            "push_attempts": 2,
            "quarantined": {
                "8c1f0b9e-0000-4000-8000-000000000005": "unparsable .metadata"
            },
            "read": {
                "downloaded": {
                    "bytes": 2048,
                    "files": 7
                },
                "unmirrored": 3
            },
            "reconciled": null,
            "schema": 1,
            "skipped": [
                "8c1f0b9e-0000-4000-8000-000000000004",
                "8c1f0b9e-0000-4000-8000-000000000016"
            ],
            "snapshot": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "summary": {
                "added": 0,
                "changed_ours": 2,
                "changed_theirs": 3,
                "deleted": 0,
                "forked": 1,
                "identical": 0,
                "kept": 0,
                "merged": 0,
                "resurrected": 0,
                "unchanged": 40
            },
            "took_over": null,
            "update": {
                "kind": "pulled"
            },
            "warnings": []
        })
    );
}

/// A status report.
#[test]
fn status_report_json() {
    let value = json::report(&Report::Status(status_report()));
    assert_eq!(
        value,
        json!({
            "behind": 4,
            "changes": {
                "8c1f0b9e-0000-4000-8000-000000000001": "added",
                "8c1f0b9e-0000-4000-8000-000000000002": "modified",
                "8c1f0b9e-0000-4000-8000-000000000003": "deleted"
            },
            "command": "status",
            "device_id": "0f0e0d0c-0000-4000-8000-0000000000c1",
            "device_name": "Desk tablet",
            "forks": [],
            "guard": {
                "allowed": false,
                "empty_snapshot": false,
                "exempt": [
                    "8c1f0b9e-0000-4000-8000-000000000017"
                ],
                "from_origin": [
                    "8c1f0b9e-0000-4000-8000-000000000014"
                ],
                "from_tablet": [
                    "8c1f0b9e-0000-4000-8000-000000000015",
                    "8c1f0b9e-0000-4000-8000-000000000016"
                ],
                "needs_consent": true
            },
            "journal_pending": true,
            "lock": {
                "owner": {
                    "boot_id": "0f0e0d0c-0000-4000-8000-0000000000b1",
                    "host": "laptop",
                    "pid": 4242,
                    "run": "run-1",
                    "started_unix": 1790000000
                },
                "state": "held"
            },
            "names": {
                "8c1f0b9e-0000-4000-8000-000000000003": "Gone notebook",
                "8c1f0b9e-0000-4000-8000-000000000014": "Removed on this tablet",
                "8c1f0b9e-0000-4000-8000-000000000015": "Old draft",
                "8c1f0b9e-0000-4000-8000-000000000016": "Second draft",
                "8c1f0b9e-0000-4000-8000-000000000017": "Cleaned up elsewhere"
            },
            "notes": [
                {
                    "doc": "8c1f0b9e-0000-4000-8000-000000000002",
                    "field": "visibleName",
                    "kind": "metadata_conflict"
                }
            ],
            "open_conflicts": [
                {
                    "copy": "8c1f0b9e-0000-4000-8000-00000000000a",
                    "name": "Lecture notes (conflict Desk tablet 2026-11-09)",
                    "original": "8c1f0b9e-0000-4000-8000-000000000009"
                }
            ],
            "origin": "ssh://git@example.invalid/library.git",
            "orphans": [],
            "quarantined": {
                "8c1f0b9e-0000-4000-8000-000000000005": "zero-length page"
            },
            "read": {
                "downloaded": {
                    "bytes": 0,
                    "files": 0
                },
                "unmirrored": 0
            },
            "repo_id": "0f0e0d0c-0000-4000-8000-0000000000a1",
            "schema": 1,
            "summary": {
                "added": 1,
                "changed_ours": 0,
                "changed_theirs": 0,
                "deleted": 0,
                "forked": 0,
                "identical": 0,
                "kept": 0,
                "merged": 0,
                "resurrected": 0,
                "unchanged": 0
            }
        })
    );
}

/// A dry run (the status nested).
#[test]
fn dry_run_report_json() {
    let value = json::report(&Report::DryRun(dry_run_report()));
    assert_eq!(
        value,
        json!({
            "command": "sync --dry-run",
            "deletes": [
                "8c1f0b9e-0000-4000-8000-000000000020"
            ],
            "schema": 1,
            "status": {
                "behind": 4,
                "changes": {
                    "8c1f0b9e-0000-4000-8000-000000000001": "added",
                    "8c1f0b9e-0000-4000-8000-000000000002": "modified",
                    "8c1f0b9e-0000-4000-8000-000000000003": "deleted"
                },
                "device_id": "0f0e0d0c-0000-4000-8000-0000000000c1",
                "device_name": "Desk tablet",
                "forks": [],
                "guard": {
                    "allowed": false,
                    "empty_snapshot": false,
                    "exempt": [
                        "8c1f0b9e-0000-4000-8000-000000000017"
                    ],
                    "from_origin": [
                        "8c1f0b9e-0000-4000-8000-000000000014"
                    ],
                    "from_tablet": [
                        "8c1f0b9e-0000-4000-8000-000000000015",
                        "8c1f0b9e-0000-4000-8000-000000000016"
                    ],
                    "needs_consent": true
                },
                "journal_pending": true,
                "lock": {
                    "owner": {
                        "boot_id": "0f0e0d0c-0000-4000-8000-0000000000b1",
                        "host": "laptop",
                        "pid": 4242,
                        "run": "run-1",
                        "started_unix": 1790000000
                    },
                    "state": "held"
                },
                "names": {
                    "8c1f0b9e-0000-4000-8000-000000000003": "Gone notebook",
                    "8c1f0b9e-0000-4000-8000-000000000014": "Removed on this tablet",
                    "8c1f0b9e-0000-4000-8000-000000000015": "Old draft",
                    "8c1f0b9e-0000-4000-8000-000000000016": "Second draft",
                    "8c1f0b9e-0000-4000-8000-000000000017": "Cleaned up elsewhere"
                },
                "notes": [
                    {
                        "doc": "8c1f0b9e-0000-4000-8000-000000000002",
                        "field": "visibleName",
                        "kind": "metadata_conflict"
                    }
                ],
                "open_conflicts": [
                    {
                        "copy": "8c1f0b9e-0000-4000-8000-00000000000a",
                        "name": "Lecture notes (conflict Desk tablet 2026-11-09)",
                        "original": "8c1f0b9e-0000-4000-8000-000000000009"
                    }
                ],
                "origin": "ssh://git@example.invalid/library.git",
                "orphans": [],
                "quarantined": {
                    "8c1f0b9e-0000-4000-8000-000000000005": "zero-length page"
                },
                "read": {
                    "downloaded": {
                        "bytes": 0,
                        "files": 0
                    },
                    "unmirrored": 0
                },
                "repo_id": "0f0e0d0c-0000-4000-8000-0000000000a1",
                "summary": {
                    "added": 1,
                    "changed_ours": 0,
                    "changed_theirs": 0,
                    "deleted": 0,
                    "forked": 0,
                    "identical": 0,
                    "kept": 0,
                    "merged": 0,
                    "resurrected": 0,
                    "unchanged": 0
                }
            },
            "would_push": true,
            "writes": [
                "8c1f0b9e-0000-4000-8000-00000000001e",
                "8c1f0b9e-0000-4000-8000-00000000001f"
            ]
        })
    );
}

/// A backup report.
#[test]
fn backup_report_json() {
    let value = json::report(&Report::Backup(backup_report()));
    assert_eq!(
        value,
        json!({
            "command": "backup",
            "orphans": [
                "8c1f0b9e-0000-4000-8000-000000000006"
            ],
            "pushed": true,
            "quarantined": {},
            "read": {
                "downloaded": {
                    "bytes": 10,
                    "files": 1
                },
                "unmirrored": 0
            },
            "schema": 1,
            "snapshot": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "took_over": {
                "previous": null,
                "reason": "broken"
            },
            "warnings": []
        })
    );
}

/// An init report.
#[test]
fn init_report_json() {
    let value = json::report(&init_report());
    assert_eq!(
        value,
        json!({
            "command": "init",
            "commit": "ffffffffffffffffffffffffffffffffffffffff",
            "config": {
                "format": 1,
                "mirror_rules": 1,
                "repo_id": "0f0e0d0c-0000-4000-8000-0000000000a1"
            },
            "origin": "/srv/library.git",
            "schema": 1
        })
    );
}

/// A remote report.
#[test]
fn remote_report_json() {
    let value = json::report(&Report::Remote(remote_report()));
    assert_eq!(
        value,
        json!({
            "command": "remote",
            "device_id": "0f0e0d0c-0000-4000-8000-0000000000c1",
            "device_name": "Desk tablet",
            "effective": "ssh://git@example.invalid/library.git",
            "origin": "ssh://git@example.invalid/library.git",
            "repo_id": "0f0e0d0c-0000-4000-8000-0000000000a1",
            "schema": 1
        })
    );
}

/// A set-url report.
#[test]
fn set_url_report_json() {
    let value = json::report(&Report::SetUrl(set_url_report()));
    assert_eq!(
        value,
        json!({
            "command": "remote set-url",
            "previous": "/old/library.git",
            "remote": {
                "device_id": "0f0e0d0c-0000-4000-8000-0000000000c1",
                "device_name": "Desk tablet",
                "effective": "ssh://git@example.invalid/library.git",
                "origin": "ssh://git@example.invalid/library.git",
                "repo_id": "0f0e0d0c-0000-4000-8000-0000000000a1"
            },
            "schema": 1,
            "took_over": null,
            "warnings": [
                {
                    "error": "connection lost",
                    "kind": "release_failed"
                }
            ]
        })
    );
}

/// A detach report.
#[test]
fn detach_report_json() {
    let value = json::report(&Report::Detach(detach_report()));
    assert_eq!(
        value,
        json!({
            "command": "detach",
            "device_id": "0f0e0d0c-0000-4000-8000-0000000000c1",
            "pruned": "refs/rmu/devices/0f0e0d0c-0000-4000-8000-0000000000c1",
            "repo_id": "0f0e0d0c-0000-4000-8000-0000000000a1",
            "schema": 1,
            "took_over": null,
            "warnings": [
                {
                    "kind": "journal_discarded"
                }
            ]
        })
    );
}

/// The deletion guard's refusal.
#[test]
fn guard_refusal_json() {
    let value = {
        let error = guard_refusal();
        json::error(Failure::of(&error), &error.to_string(), Some(&error))
    };
    assert_eq!(
        value,
        json!({
            "error": {
                "empty_snapshot": true,
                "exit_code": 3,
                "from_origin": [
                    "8c1f0b9e-0000-4000-8000-000000000001",
                    "8c1f0b9e-0000-4000-8000-000000000002"
                ],
                "from_tablet": [
                    "8c1f0b9e-0000-4000-8000-000000000003"
                ],
                "kind": "deletions_need_consent",
                "message": "this sync would delete 2 document(s) from origin and 1 from the tablet (the tablet looks empty); nothing was pushed. Rerun with --allow-deletions if that is intended",
                "names": {
                    "8c1f0b9e-0000-4000-8000-000000000001": "Deleted paper",
                    "8c1f0b9e-0000-4000-8000-000000000002": "Deleted notes",
                    "8c1f0b9e-0000-4000-8000-000000000003": "Old draft"
                }
            },
            "schema": 1
        })
    );
}

/// The tablet did not answer.
#[test]
fn unreachable_error_json() {
    let error = Error::Unreachable("Timeout, server tablet not responding.".to_string());
    let value = json::error(Failure::of(&error), &error.to_string(), Some(&error));
    assert_eq!(value["error"]["kind"], "unreachable");
    assert_eq!(value["error"]["exit_code"], 8);
}

/// Any other error.
#[test]
fn other_error_json() {
    let value = json::error(Failure::Error, "ssh: connection refused", None);
    assert_eq!(
        value,
        json!({
            "error": {
                "exit_code": 1,
                "kind": "error",
                "message": "ssh: connection refused"
            },
            "schema": 1
        })
    );
}

/// The library's history.
#[test]
fn log_report_json() {
    let value = json::report(&Report::Log(log_report()));
    assert_eq!(
        value,
        json!({
            "command": "log",
            "entries": [
                {
                    "added": [
                        "8c1f0b9e-0000-4000-8000-000000000004"
                    ],
                    "author": "rmu",
                    "changed": [],
                    "commit": "dddddddddddddddddddddddddddddddddddddddd",
                    "consented": [],
                    "deleted": [],
                    "device": null,
                    "forks": [],
                    "kind": "restore",
                    "names": {
                        "8c1f0b9e-0000-4000-8000-000000000003": "Old",
                        "8c1f0b9e-0000-4000-8000-000000000004": "Old (restored 2026-11-09)"
                    },
                    "notes": [],
                    "restored": [
                        {
                            "copy": "8c1f0b9e-0000-4000-8000-000000000004",
                            "from": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                            "original": "8c1f0b9e-0000-4000-8000-000000000003"
                        }
                    ],
                    "subject": "rmu hub: restore \"Old (restored 2026-11-09)\"",
                    "time": 1794233000
                },
                {
                    "added": [
                        "8c1f0b9e-0000-4000-8000-000000000002"
                    ],
                    "author": "Desk tablet",
                    "changed": [
                        "8c1f0b9e-0000-4000-8000-000000000001"
                    ],
                    "commit": "cccccccccccccccccccccccccccccccccccccccc",
                    "consented": [
                        "8c1f0b9e-0000-4000-8000-000000000003"
                    ],
                    "deleted": [
                        "8c1f0b9e-0000-4000-8000-000000000003"
                    ],
                    "device": {
                        "firmware": "3.28.0",
                        "id": "0f0e0d0c-0000-4000-8000-0000000000c1",
                        "model": "reMarkable Fake",
                        "name": "Desk tablet"
                    },
                    "forks": [
                        {
                            "copy": "8c1f0b9e-0000-4000-8000-000000000002",
                            "original": "8c1f0b9e-0000-4000-8000-000000000001"
                        }
                    ],
                    "kind": "sync",
                    "names": {
                        "8c1f0b9e-0000-4000-8000-000000000001": "Notes",
                        "8c1f0b9e-0000-4000-8000-000000000002": "Notes (conflict Desk tablet 2026-11-09)",
                        "8c1f0b9e-0000-4000-8000-000000000003": "Old"
                    },
                    "notes": [
                        "8c1f0b9e-0000-4000-8000-000000000001: \"visibleName\" changed on both sides"
                    ],
                    "restored": [],
                    "subject": "rmu hub: sync Desk tablet",
                    "time": 1794233000
                },
                {
                    "added": [],
                    "author": "rmu",
                    "changed": [],
                    "commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "consented": [],
                    "deleted": [],
                    "device": null,
                    "forks": [],
                    "kind": "init",
                    "names": {},
                    "notes": [],
                    "restored": [],
                    "subject": "rmu hub: init",
                    "time": 1794233000
                }
            ],
            "main": "dddddddddddddddddddddddddddddddddddddddd",
            "more": 5,
            "origin": "ssh://git@example.invalid/library.git",
            "repo_id": "0f0e0d0c-0000-4000-8000-0000000000a1",
            "schema": 1,
            "tablet": {
                "base": "cccccccccccccccccccccccccccccccccccccccc",
                "device_id": "0f0e0d0c-0000-4000-8000-0000000000c1",
                "device_name": "Desk tablet",
                "overrides": 2
            }
        })
    );
}

/// A document's history.
#[test]
fn doc_log_report_json() {
    let value = json::report(&Report::DocLog(doc_log_report()));
    assert_eq!(
        value,
        json!({
            "command": "log",
            "doc": "8c1f0b9e-0000-4000-8000-000000000001",
            "entries": [
                {
                    "author": "Desk tablet",
                    "commit": "dddddddddddddddddddddddddddddddddddddddd",
                    "events": [
                        {
                            "kind": "changed"
                        },
                        {
                            "copy": "8c1f0b9e-0000-4000-8000-000000000002",
                            "kind": "forked_into"
                        }
                    ],
                    "kind": "sync",
                    "subject": "rmu hub: sync Desk tablet",
                    "time": 1794233000,
                    "version": "9999999999999999999999999999999999999999"
                },
                {
                    "author": "Desk tablet",
                    "commit": "cccccccccccccccccccccccccccccccccccccccc",
                    "events": [
                        {
                            "from": "Draft",
                            "kind": "renamed",
                            "to": "Notes"
                        },
                        {
                            "from": {
                                "parent": "",
                                "path": "/"
                            },
                            "kind": "moved",
                            "to": {
                                "parent": "8c1f0b9e-0000-4000-8000-000000000028",
                                "path": "/Projects"
                            }
                        }
                    ],
                    "kind": "sync",
                    "subject": "rmu hub: sync Desk tablet",
                    "time": 1794233000,
                    "version": "9999999999999999999999999999999999999999"
                },
                {
                    "author": "Desk tablet",
                    "commit": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "events": [
                        {
                            "copy": "8c1f0b9e-0000-4000-8000-000000000004",
                            "from": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                            "kind": "restored_as"
                        }
                    ],
                    "kind": "sync",
                    "subject": "rmu hub: sync Desk tablet",
                    "time": 1794233000,
                    "version": "9999999999999999999999999999999999999999"
                },
                {
                    "author": "Desk tablet",
                    "commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "events": [
                        {
                            "kind": "added",
                            "name": "Draft",
                            "place": {
                                "parent": "",
                                "path": "/"
                            }
                        }
                    ],
                    "kind": "sync",
                    "subject": "rmu hub: sync Desk tablet",
                    "time": 1794233000,
                    "version": "9999999999999999999999999999999999999999"
                }
            ],
            "main": "dddddddddddddddddddddddddddddddddddddddd",
            "name": "Notes",
            "origin": "ssh://git@example.invalid/library.git",
            "place": {
                "parent": "8c1f0b9e-0000-4000-8000-000000000028",
                "path": "/Projects"
            },
            "repo_id": "0f0e0d0c-0000-4000-8000-0000000000a1",
            "schema": 1,
            "tablet": {
                "base": "cccccccccccccccccccccccccccccccccccccccc",
                "device_id": "0f0e0d0c-0000-4000-8000-0000000000c1",
                "device_name": "Desk tablet",
                "overrides": 2
            }
        })
    );
}

/// What `restore` committed.
#[test]
fn restore_report_json() {
    let value = json::report(&Report::Restore(restore_report()));
    assert_eq!(
        value,
        json!({
            "at": "dddddddddddddddddddddddddddddddddddddddd",
            "command": "restore",
            "commit": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "copy": "8c1f0b9e-0000-4000-8000-000000000004",
            "from": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "name": "Old (restored 2026-11-09)",
            "original": "8c1f0b9e-0000-4000-8000-000000000003",
            "original_name": "Old",
            "place": {
                "parent": "8c1f0b9e-0000-4000-8000-000000000028",
                "path": "/Projects"
            },
            "push_attempts": 2,
            "schema": 1
        })
    );
}

/// What `add` committed.
#[test]
fn add_report_json() {
    let value = json::report(&Report::Add(add_report(Some(3))));
    assert_eq!(
        value,
        json!({
            "bytes": 2048,
            "command": "add",
            "commit": "ffffffffffffffffffffffffffffffffffffffff",
            "doc": "8c1f0b9e-0000-4000-8000-000000000005",
            "file_type": "pdf",
            "name": "Report",
            "pages": 3,
            "place": {
                "parent": "",
                "path": "/"
            },
            "push_attempts": 1,
            "schema": 1
        })
    );
}
