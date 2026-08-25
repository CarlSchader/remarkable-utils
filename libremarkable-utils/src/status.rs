//! Device system status: model, firmware, CPU/RAM/disk, battery.
//!
//! All facts are gathered in **one SSH round trip** (marker-sectioned
//! script, same technique as the document listing) and parsed
//! host-side by pure, unit-tested functions. Every field is optional:
//! rM1/rM2/Paper Pro expose different subsets, and a missing file
//! means an omitted row, never an error.

use serde::Serialize;

use crate::xochitl::{self, Item};

/// One mounted filesystem's usage (from `df -kP`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiskUsage {
    pub mount: String,
    pub total_kib: u64,
    pub used_kib: u64,
    pub available_kib: u64,
}

/// Battery state (from `/sys/class/power_supply`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Battery {
    pub percent: u8,
    /// Kernel-reported status, e.g. `Charging` / `Discharging` / `Full`.
    pub status: Option<String>,
}

/// Logical document counts from the xochitl listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct DocumentCounts {
    pub documents: usize,
    pub folders: usize,
    pub trashed: usize,
}

/// A best-effort snapshot of the tablet's system state.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SystemStatus {
    pub model: Option<String>,
    pub serial: Option<String>,
    /// reMarkable software version (update.conf / os-release).
    pub os_version: Option<String>,
    /// Build identifier (`/etc/version`).
    pub os_build: Option<String>,
    pub kernel: Option<String>,
    pub arch: Option<String>,
    pub uptime_secs: Option<f64>,
    pub load_average: Option<(f64, f64, f64)>,
    pub cpu_count: Option<u32>,
    pub mem_total_kib: Option<u64>,
    pub mem_available_kib: Option<u64>,
    pub disks: Vec<DiskUsage>,
    pub battery: Option<Battery>,
    /// `systemctl is-active xochitl` verbatim (`active`, `inactive`, ...).
    pub xochitl: Option<String>,
    /// Filled in from the document listing, not the status script.
    pub documents: Option<DocumentCounts>,
}

/// The remote gathering script. Every read is best-effort
/// (`2>/dev/null`); the script always exits 0.
pub fn status_script(marker: &str) -> String {
    let marker = crate::ssh::shell_quote(marker);
    format!(
        "sec() {{ printf '\\n%s %s\\n' {marker} \"$1\"; }}\n\
         sec model; cat /sys/devices/soc0/machine /proc/device-tree/model 2>/dev/null\n\
         sec serial; cat /proc/device-tree/serial-number 2>/dev/null\n\
         sec update-conf; cat /usr/share/remarkable/update.conf 2>/dev/null\n\
         sec os-release; cat /etc/os-release 2>/dev/null\n\
         sec build; cat /etc/version 2>/dev/null\n\
         sec kernel; uname -r 2>/dev/null; uname -m 2>/dev/null\n\
         sec uptime; cat /proc/uptime 2>/dev/null\n\
         sec loadavg; cat /proc/loadavg 2>/dev/null\n\
         sec cpus; grep -c ^processor /proc/cpuinfo 2>/dev/null\n\
         sec meminfo; cat /proc/meminfo 2>/dev/null\n\
         sec df; df -kP /home / 2>/dev/null\n\
         sec battery; for d in /sys/class/power_supply/*; do\n\
         [ -e \"$d/capacity\" ] || continue\n\
         printf '%s %s\\n' \"$(cat \"$d/capacity\" 2>/dev/null)\" \"$(cat \"$d/status\" 2>/dev/null)\"\n\
         done\n\
         sec xochitl; systemctl is-active xochitl 2>/dev/null\n\
         true\n"
    )
}

/// Parse the status script output. Pure.
pub fn parse_status(marker: &str, output: &str) -> SystemStatus {
    let sections = split_sections(marker, output);
    let section = |name: &str| {
        sections
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, b)| b.as_str())
    };

    let mut kernel_lines = section("kernel")
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    let (kernel, arch) = (
        kernel_lines.next().map(str::to_string),
        kernel_lines.next().map(str::to_string),
    );

    SystemStatus {
        model: section("model").and_then(first_dt_string),
        serial: section("serial").and_then(first_dt_string),
        os_version: section("update-conf")
            .and_then(|body| key_value(body, "REMARKABLE_RELEASE_VERSION"))
            .or_else(|| section("os-release").and_then(|body| key_value(body, "VERSION_ID"))),
        os_build: section("build").and_then(first_line),
        kernel,
        arch,
        uptime_secs: section("uptime")
            .and_then(|body| body.split_whitespace().next()?.parse().ok()),
        load_average: section("loadavg").and_then(|body| {
            let mut parts = body
                .split_whitespace()
                .filter_map(|p| p.parse::<f64>().ok());
            Some((parts.next()?, parts.next()?, parts.next()?))
        }),
        cpu_count: section("cpus").and_then(|body| body.trim().parse().ok()),
        mem_total_kib: section("meminfo").and_then(|body| meminfo_field(body, "MemTotal")),
        mem_available_kib: section("meminfo").and_then(|body| meminfo_field(body, "MemAvailable")),
        disks: section("df").map(parse_df).unwrap_or_default(),
        battery: section("battery").and_then(parse_battery),
        xochitl: section("xochitl").and_then(first_line),
        documents: None,
    }
}

/// Count documents/folders (excluding trash) and trashed items.
pub fn document_counts(items: &[Item]) -> DocumentCounts {
    items
        .iter()
        .fold(DocumentCounts::default(), |mut counts, item| {
            if xochitl::is_descendant(items, &item.uuid, xochitl::TRASH_PARENT) {
                counts.trashed += 1;
            } else if item.is_folder() {
                counts.folders += 1;
            } else {
                counts.documents += 1;
            }
            counts
        })
}

/// Render a human-readable report; rows with no data are omitted.
pub fn render(status: &SystemStatus) -> Vec<String> {
    let row = |label: &str, value: String| format!("{label:<11}{value}");
    let mut lines = Vec::new();

    if let Some(model) = &status.model {
        lines.push(row("Model:", model.clone()));
    }
    if let Some(serial) = &status.serial {
        lines.push(row("Serial:", serial.clone()));
    }
    match (&status.os_version, &status.os_build) {
        (Some(version), Some(build)) => {
            lines.push(row("OS:", format!("{version} (build {build})")));
        }
        (Some(version), None) => lines.push(row("OS:", version.clone())),
        (None, Some(build)) => lines.push(row("OS:", format!("build {build}"))),
        (None, None) => {}
    }
    match (&status.kernel, &status.arch) {
        (Some(kernel), Some(arch)) => lines.push(row("Kernel:", format!("{kernel} ({arch})"))),
        (Some(kernel), None) => lines.push(row("Kernel:", kernel.clone())),
        _ => {}
    }
    if let Some(uptime) = status.uptime_secs {
        lines.push(row("Uptime:", format_duration(uptime)));
    }
    match (status.cpu_count, status.load_average) {
        (Some(cpus), Some((one, five, fifteen))) => lines.push(row(
            "CPU:",
            format!("{cpus} core(s), load {one:.2} {five:.2} {fifteen:.2}"),
        )),
        (Some(cpus), None) => lines.push(row("CPU:", format!("{cpus} core(s)"))),
        (None, Some((one, five, fifteen))) => {
            lines.push(row("CPU:", format!("load {one:.2} {five:.2} {fifteen:.2}")));
        }
        (None, None) => {}
    }
    if let (Some(total), Some(available)) = (status.mem_total_kib, status.mem_available_kib) {
        lines.push(row(
            "Memory:",
            format!(
                "{} used / {} total",
                format_kib(total.saturating_sub(available)),
                format_kib(total)
            ),
        ));
    }
    status.disks.iter().for_each(|disk| {
        let percent = (disk.used_kib * 100)
            .checked_div(disk.total_kib)
            .unwrap_or(0);
        lines.push(row(
            "Storage:",
            format!(
                "{} {} used / {} ({percent}%)",
                disk.mount,
                format_kib(disk.used_kib),
                format_kib(disk.total_kib)
            ),
        ));
    });
    if let Some(battery) = &status.battery {
        let suffix = battery
            .status
            .as_deref()
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        lines.push(row("Battery:", format!("{}%{suffix}", battery.percent)));
    }
    if let Some(xochitl) = &status.xochitl {
        lines.push(row("xochitl:", xochitl.clone()));
    }
    if let Some(counts) = &status.documents {
        lines.push(row(
            "Documents:",
            format!(
                "{} document(s), {} folder(s), {} in trash",
                counts.documents, counts.folders, counts.trashed
            ),
        ));
    }
    lines
}

// ---------------------------------------------------------------------------
// Parsing helpers (pure)
// ---------------------------------------------------------------------------

fn split_sections(marker: &str, output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .fold(Vec::<(String, String)>::new(), |mut sections, line| {
            match line
                .strip_prefix(marker)
                .and_then(|rest| rest.strip_prefix(' '))
            {
                Some(name) => sections.push((name.trim().to_string(), String::new())),
                None => {
                    if let Some((_, body)) = sections.last_mut() {
                        body.push_str(line);
                        body.push('\n');
                    }
                }
            }
            sections
        })
}

/// Device-tree string files are NUL-terminated (and sometimes
/// NUL-padded); strip that before use.
fn first_dt_string(body: &str) -> Option<String> {
    body.lines()
        .map(|line| line.trim_matches(['\0', ' ', '\t', '\r']))
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn first_line(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// `KEY=value` lookup (update.conf / os-release style), stripping quotes.
fn key_value(body: &str, key: &str) -> Option<String> {
    body.lines()
        .filter_map(|line| line.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

/// `MemTotal:        1892456 kB` → 1892456.
fn meminfo_field(body: &str, field: &str) -> Option<u64> {
    body.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim() == field)
        .and_then(|(_, rest)| rest.split_whitespace().next()?.parse().ok())
}

/// Parse `df -kP` output (header + one line per filesystem),
/// deduplicating by mount point (`df /home /` reports one filesystem
/// twice when they share a device).
fn parse_df(body: &str) -> Vec<DiskUsage> {
    let mut seen = std::collections::HashSet::new();
    body.lines()
        .skip(1) // header
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [_, total, used, available, _, mount @ ..] = fields.as_slice() else {
                return None;
            };
            let mount = mount.join(" ");
            if mount.is_empty() || !seen.insert(mount.clone()) {
                return None;
            }
            Some(DiskUsage {
                mount,
                total_kib: total.parse().ok()?,
                used_kib: used.parse().ok()?,
                available_kib: available.parse().ok()?,
            })
        })
        .collect()
}

/// `86 Discharging` lines (one per power supply with a capacity).
fn parse_battery(body: &str) -> Option<Battery> {
    body.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let percent: u8 = parts.next()?.parse().ok()?;
        (percent <= 100).then(|| Battery {
            percent,
            status: parts.next().map(str::to_string),
        })
    })
}

/// `279123.45` seconds → `3d 5h 32m`.
fn format_duration(secs: f64) -> String {
    let total_minutes = (secs / 60.0) as u64;
    let (days, hours, minutes) = (
        total_minutes / (24 * 60),
        (total_minutes / 60) % 24,
        total_minutes % 60,
    );
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h {minutes}m"),
    }
}

/// KiB → human-readable (`412 MiB`, `1.9 GiB`).
fn format_kib(kib: u64) -> String {
    let bytes = kib as f64 * 1024.0;
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let (value, unit) = UNITS
        .iter()
        .enumerate()
        .map(|(i, unit)| (bytes / 1024f64.powi(i as i32 + 1), unit))
        .take_while(|(value, _)| *value >= 1.0)
        .last()
        .unwrap_or((kib as f64, &"KiB"));
    if value >= 10.0 {
        format!("{value:.0} {unit}")
    } else {
        format!("{value:.1} {unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xochitl::ItemKind;

    /// A Paper-Pro-shaped fixture with a fabricated serial.
    fn fixture() -> String {
        let m = "===M===";
        format!(
            "\n{m} model\nreMarkable Ferrari\u{0}\n\
             \n{m} serial\nRM100-000-00000\u{0}\u{0}\n\
             \n{m} update-conf\n[General]\nREMARKABLE_RELEASE_VERSION=3.20.0.92\n\
             \n{m} os-release\nID=codex\nVERSION_ID=\"3.20.0\"\n\
             \n{m} build\n20240915163045\n\
             \n{m} kernel\n6.1.36-rm\naarch64\n\
             \n{m} uptime\n279123.45 401234.00\n\
             \n{m} loadavg\n0.15 0.10 0.05 1/123 4567\n\
             \n{m} cpus\n2\n\
             \n{m} meminfo\nMemTotal:        1892456 kB\nMemFree:          123456 kB\nMemAvailable:    1479844 kB\n\
             \n{m} df\nFilesystem 1024-blocks Used Available Capacity Mounted on\n/dev/mmcblk0p4 6712344 2201234 4511110 33% /home\n/dev/mmcblk0p2 505576 288012 217564 57% /\n\
             \n{m} battery\n86 Discharging\n\
             \n{m} xochitl\nactive\n"
        )
    }

    #[test]
    fn parses_full_fixture() {
        let status = parse_status("===M===", &fixture());
        assert_eq!(status.model.as_deref(), Some("reMarkable Ferrari"));
        // NUL padding stripped from device-tree strings.
        assert_eq!(status.serial.as_deref(), Some("RM100-000-00000"));
        assert_eq!(status.os_version.as_deref(), Some("3.20.0.92"));
        assert_eq!(status.os_build.as_deref(), Some("20240915163045"));
        assert_eq!(status.kernel.as_deref(), Some("6.1.36-rm"));
        assert_eq!(status.arch.as_deref(), Some("aarch64"));
        assert_eq!(status.uptime_secs, Some(279123.45));
        assert_eq!(status.load_average, Some((0.15, 0.10, 0.05)));
        assert_eq!(status.cpu_count, Some(2));
        assert_eq!(status.mem_total_kib, Some(1_892_456));
        assert_eq!(status.mem_available_kib, Some(1_479_844));
        assert_eq!(status.disks.len(), 2);
        assert_eq!(status.disks[0].mount, "/home");
        assert_eq!(status.disks[0].total_kib, 6_712_344);
        assert_eq!(
            status.battery,
            Some(Battery {
                percent: 86,
                status: Some("Discharging".to_string())
            })
        );
        assert_eq!(status.xochitl.as_deref(), Some("active"));
    }

    #[test]
    fn missing_sections_yield_none_not_errors() {
        let status = parse_status("===M===", "\n===M=== model\n\n===M=== battery\n");
        assert_eq!(status.model, None);
        assert_eq!(status.battery, None);
        assert_eq!(status.os_version, None);
        assert!(status.disks.is_empty());
    }

    #[test]
    fn os_release_is_the_version_fallback() {
        let out = "\n===M=== os-release\nVERSION_ID=\"3.5.2\"\n";
        let status = parse_status("===M===", out);
        assert_eq!(status.os_version.as_deref(), Some("3.5.2"));
    }

    #[test]
    fn df_dedupes_shared_mounts() {
        let body = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                    /dev/root 100 50 50 50% /\n\
                    /dev/root 100 50 50 50% /\n";
        assert_eq!(parse_df(body).len(), 1);
    }

    #[test]
    fn counts_split_trash_from_live_items() {
        let item = |uuid: &str, parent: &str, kind: ItemKind| Item {
            uuid: uuid.to_string(),
            visible_name: uuid.to_string(),
            parent: parent.to_string(),
            kind,
            file_type: None,
            created_time: 0,
            last_modified: 0,
            size_bytes: None,
        };
        let items = vec![
            item("f", "", ItemKind::Folder),
            item("d1", "f", ItemKind::Document),
            item("t", xochitl::TRASH_PARENT, ItemKind::Folder),
            item("td", "t", ItemKind::Document), // nested in trashed folder
        ];
        assert_eq!(
            document_counts(&items),
            DocumentCounts {
                documents: 1,
                folders: 1,
                trashed: 2
            }
        );
    }

    #[test]
    fn render_omits_missing_rows() {
        let full = parse_status("===M===", &fixture());
        let lines = render(&full);
        assert!(lines.iter().any(|l| l.starts_with("Serial:")));
        assert!(lines.iter().any(|l| l.contains("3d 5h 32m")));
        assert!(lines.iter().any(|l| l.contains("2 core(s), load 0.15")));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("/home 2.1 GiB used / 6.4 GiB (32%)"))
        );
        assert!(lines.iter().any(|l| l.contains("86% (Discharging)")));

        let empty = render(&SystemStatus::default());
        assert!(empty.is_empty());
    }

    #[test]
    fn duration_and_size_formatting() {
        assert_eq!(format_duration(59.0), "0m");
        assert_eq!(format_duration(3_660.0), "1h 1m");
        assert_eq!(format_duration(90_000.0), "1d 1h 0m");
        assert_eq!(format_kib(512), "512 KiB");
        assert_eq!(format_kib(2048), "2.0 MiB");
        assert_eq!(format_kib(1_992_294), "1.9 GiB");
    }
}
