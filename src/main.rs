use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Parser, Subcommand};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
enum AppError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("command '{0}' failed: {1}")]
    Command(String, String),

    #[error("failed to spawn '{0}': {1}")]
    Spawn(String, #[source] io::Error),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("fstab verification failed:\n{0}")]
    VerifyFailed(String),

    #[error("refusing to overwrite {0} without --force")]
    RefuseOverwrite(String),
}

type Result<T> = std::result::Result<T, AppError>;

#[derive(Parser, Debug)]
#[command(name = "mk", version, about = "A helper for generating /etc/fstab entries")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Devices,
    Manual,
    Auto {
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    Save {
        #[arg(short, long, default_value = "fstab.draft")]
        output: PathBuf,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Deserialize)]
struct LsblkOutput {
    blockdevices: Vec<BlockDevice>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_coe)]
struct BlockDevice {
    name: String,
    #[serde(rename = "type", default)]
    dev_type: Option<String>,
    #[serde(default)]
    fstype: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    mountpoint: Option<String>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    children: Vec<BlockDevice>,
}

fn lsblk_devices() -> Result<Vec<BlockDevice>> {
    let out = Command::new("lsblk")
        .args([
            "-J",
            "-o",
            "NAME,TYPE,FSTYPE,UUID,LABEL,MOUNTPOINT,SIZE",
        ])
        .output()
        .map_err(|e| AppError::Spawn("lsblk".into(), e))?;

    if !out.status.success() {
        return Err(AppError::Command(
            "lsblk".into(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }

    let parsed: LsblkOutput = serde_json::from_slice(&out.stdout)?;
    Ok(parsed.blockdevices)
}

fn flatten_devices(devs: &[BlockDevice], acc: &mut Vec<BlockDevice>) {
    for d in devs {
        acc.push(d.clone());
        flatten_devices(&d.children, acc);
    }
}

fn print_devices() -> Result<()> {
    let mut all = Vec::new();
    flatten_devices(&lsblk_devices()?, &mut all);

    println!(
        "{:<20} {:<10} {:<10} {:<38} {:<20} {}",
        "NAME", "TYPE", "FSTYPE", "UUID", "MOUNTPOINT", "SIZE"
    );
    println!("{}", "-".repeat(120));
    for d in all {
        // Show only device types that make sense to mount
        if matches!(d.dev_type.as_deref(), Some("part") | Some("disk") | Some("lvm") | Some("crypt")) {
            println!(
                "{:<20} {:<10} {:<10} {:<38} {:<20} {}",
                d.name,
                d.dev_type.as_deref().unwrap_or("-"),
                d.fstype.as_deref().unwrap_or("-"),
                d.uuid.as_deref().unwrap_or("-"),
                d.mountpoint.as_deref().unwrap_or("-"),
                d.size.as_deref().unwrap_or("-"),
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct FstabEntry {
    spec: String,
    mountpoint: String,
    fstype: String,
    options: Vec<String>,
    dump: u32,
    pass: u32,
    comment: Option<String>,
}

impl FstabEntry {
    fn new(spec: String, mountpoint: String, fstype: String) -> Self {
        let pass = if mountpoint == "/" { 1 } else { 2 };

        Self {
            spec,
            mountpoint,
            fstype,
            options: vec!["defaults".into()],
            dump: 0,
            pass,
            comment: None,
        }
    }
}

/// Escape an fstab field: spaces, tabs, newlines.
/// See fstab(5): \040 space, \011 tab, \012 newline, \\ backslash.
fn escape_fstab_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            ' ' => out.push_str("\\040"),
            '\t' => out.push_str("\\011"),
            '\n' => out.push_str("\\012"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out
}

impl std::fmt::Display for FstabEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(c) = &self.comment {
            writeln!(f, "# {}", c)?;
        }
        writeln!(
            f,
            "{}\t{}\t{}\t{}\t{}\t{}",
            escape_fstab_field(&self.spec),
            escape_fstab_field(&self.mountpoint),
            escape_fstab_field(&self.fstype),
            escape_fstab_field(&self.options.join(",")),
            self.dump,
            self.pass,
        )
    }
}

#[derive(Debug, Deserialize)]
struct FindmntOutput {
    filesystems: Vec<FindmntRow>,
}

#[derive(Debug, Deserialize)]
struct FindmntRow {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    fstype: Option<String>,
    #[serde(default)]
    options: Option<String>,
}

fn findmnt_current() -> Result<Vec<FindmntRow>> {
    let out = Command::new("findmnt")
        .args(["-J", "-l", "-o", "SOURCE,TARGET,FSTYPE,OPTIONS"])
        .output()
        .map_err(|e| AppError::Spawn("findmnt".into(), e))?;

    if !out.status.success() {
        return Err(AppError::Command(
            "findmnt".into(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }

    let parsed: FindmntOutput = serde_json::from_slice(&out.stdout)?;
    Ok(parsed.filesystems)
}

const VIRTUAL_FSTYPES: &[&str] = &[
    "proc", "sysfs", "devtmpfs", "devpts", "tmpfs", "cgroup", "cgroup2",
    "pstore", "securityfs", "debugfs", "tracefs", "configfs", "fusectl",
    "mqueue", "hugetlbfs", "bpf", "autofs", "binfmt_misc", "ramfs",
    "overlay", "squashfs", "nsfs",
];

fn build_auto_entries() -> Result<Vec<FstabEntry>> {
    let rows = findmnt_current()?;
    let mut entries = Vec::new();
    let mut seen_mountpoints = HashSet::new();

    for row in rows {
        let (Some(source), Some(target), Some(fstype)) = (row.source, row.target, row.fstype) else {
            continue;
        };
        if VIRTUAL_FSTYPES.contains(&fstype.as_str()) {
            continue;
        }
        if target.starts_with("/snap") || target.starts_with("/run") {
            continue;
        }
        if !seen_mountpoints.insert(target.clone()) {
            continue;
        }

        let spec = if source.starts_with("/dev/") {
            uuid_for_source(&source).unwrap_or(source.clone())
        } else {
            source.clone()
        };

        let mut entry = FstabEntry::new(spec, target.clone(), fstype);
        entry.comment = Some(format!("auto-generated for {}", target));

        if let Some(opts) = row.options {
            let filtered: Vec<String> = opts
                .split(',')
                .filter(|o| !matches!(*o, "rw" | "ro"))
                .map(String::from)
                .collect();
            if !filtered.is_empty() {
                entry.options = filtered;
            }
        }
        entries.push(entry);
    }
    Ok(entries)
}

fn uuid_for_source(dev: &str) -> Option<String> {
    let name = Path::new(dev).file_name()?.to_str()?;
    let devs = lsblk_devices().ok()?;
    let mut all = Vec::new();
    flatten_devices(&devs, &mut all);
    all.into_iter()
        .find(|d| d.name == name)
        .and_then(|d| d.uuid)
        .map(|u| format!("UUID={}", u))
}

fn prompt(msg: &str) -> Result<String> {
    print!("{}: ", msg);
    io::stdout().flush()?;
    let mut s = String::new();
    io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

fn manual_entry() -> Result<FstabEntry> {
    let name = prompt("Device name from lsblk (e.g. sda1)")?;
    if name.is_empty() {
        return Err(AppError::InvalidInput("empty device name".into()));
    }

    let devs = lsblk_devices()?;
    let mut all = Vec::new();
    flatten_devices(&devs, &mut all);
    let dev = all
        .into_iter()
        .find(|d| d.name == name)
        .ok_or_else(|| AppError::InvalidInput(format!("device {} not found", name)))?;

    let spec = dev
        .uuid
        .as_ref()
        .map(|u| format!("UUID={}", u))
        .unwrap_or_else(|| format!("/dev/{}", dev.name));

    let default_fs = dev.fstype.clone().unwrap_or_else(|| "ext4".into());
    let fstype = {
        let s = prompt(&format!("Filesystem [{}]", default_fs))?;
        if s.is_empty() { default_fs } else { s }
    };

    let mountpoint = {
        let s = prompt("Mount point")?;
        if s.is_empty() {
            return Err(AppError::InvalidInput("empty mount point".into()));
        }
        s
    };
    if !mountpoint.starts_with('/') {
        return Err(AppError::InvalidInput(
            "mount point must be an absolute path".into(),
        ));
    }

    let opts = {
        let s = prompt("Options [defaults]")?;
        if s.is_empty() {
            vec!["defaults".into()]
        } else {
            s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
        }
    };

    let mut entry = FstabEntry::new(spec, mountpoint, fstype);
    entry.options = opts;
    Ok(entry)
}

fn verify_fstab(path: &Path) -> Result<()> {
    let out = Command::new("findmnt")
        .args(["--verify", "--tab-file"])
        .arg(path)
        .output()
        .map_err(|e| AppError::Spawn("findmnt --verify".into(), e))?;

    if out.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    match out.status.code() {
        Some(1) => {
            eprintln!("[findmnt --verify warning]\n{}{}", stdout, stderr);
            Ok(())
        }
        _ => Err(AppError::VerifyFailed(format!("{}{}", stdout, stderr))),
    }
}

fn write_with_backup(path: &Path, content: &str, force: bool) -> Result<()> {
    let is_system = path == Path::new("/etc/fstab");
    if path.exists() && !force {
        if is_system {
            return Err(AppError::RefuseOverwrite(path.display().to_string()));
        }
    }

    if path.exists() {
        let bak = path.with_extension(format!(
            "{}.bak",
            path.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));
        fs::copy(path, &bak)?;
        eprintln!("[backup] {}", bak.display());
    }

    // Write to a temporary file, verify, then rename
    let tmp = path.with_extension("tmp-mkfstab");
    fs::write(&tmp, content)?;

    if let Err(e) = verify_fstab(&tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    fs::rename(&tmp, path)?;
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Cmd::Devices => {
            print_devices()?;
        }

        Cmd::Manual => {
            let entry = manual_entry()?;
            print!("{}", entry);
        }

        Cmd::Auto { output } => {
            let entries = build_auto_entries()?;
            if entries.is_empty() {
                eprintln!("[!] no suitable mounts found");
            }
            let mut buf = String::new();
            buf.push_str("# Generated by mk-fstab --auto\n");
            for e in &entries {
                buf.push_str(&e.to_string());
            }
            if let Some(p) = output {
                write_with_backup(&p, &buf, false)?;
                eprintln!("[ok] written to {}", p.display());
            } else {
                print!("{}", buf);
            }
        }

        Cmd::Save { output, force } => {
            // Save uses the current auto set as content
            let entries = build_auto_entries()?;
            let mut buf = String::new();
            buf.push_str("# Generated by mk-fstab save\n");
            for e in &entries {
                buf.push_str(&e.to_string());
            }

            if !force {
                eprintln!(
                    "[!] About to overwrite {} (use --force to confirm for system paths)",
                    output.display()
                );
            }
            write_with_backup(&output, &buf, force)?;
            eprintln!("[ok] written to {}", output.display());
        }
    }

    Ok(())
}