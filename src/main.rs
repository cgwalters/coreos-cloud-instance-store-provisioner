//! Automatically set up a filesystem for instance-local storage
//! and redirect desired ephemeral paths to it.
//! https://github.com/coreos/ignition/issues/1126

use anyhow::{anyhow, bail, Context, Result};
use openat_ext::OpenatDirExt;
use serde_derive::Deserialize;
use std::borrow::Cow;
use std::fs::create_dir;
use std::path::Path;
use std::process::Command;

const CONFIG_PATH: &str = "/etc/coreos-cloud-instance-store-provisioner.yaml";
const MOUNTPOINT: &str = "/var/mnt/instance-storage";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Config {
    directories: Vec<String>,
}

pub(crate) trait CommandRunExt {
    fn run(&mut self) -> Result<()>;
}

impl CommandRunExt for Command {
    fn run(&mut self) -> Result<()> {
        let r = self.status()?;
        if !r.success() {
            bail!("Child [{:?}] exited: {}", self, r);
        }
        Ok(())
    }
}

mod coreos {
    use super::*;

    /// Path to kernel command-line (requires procfs mount).
    const CMDLINE_PATH: &str = "/proc/cmdline";
    /// Platform key.
    const CMDLINE_PLATFORM_FLAG: &str = "ignition.platform.id";

    // Find OEM ID flag value in cmdline string.
    fn find_flag_value(flagname: &str, cmdline: &str) -> Option<String> {
        // split the contents into elements and keep key-value tuples only.
        let params: Vec<(&str, &str)> = cmdline
            .split(' ')
            .filter_map(|s| {
                let kv: Vec<&str> = s.splitn(2, '=').collect();
                match kv.len() {
                    2 => Some((kv[0], kv[1])),
                    _ => None,
                }
            })
            .collect();

        // find the oem flag
        for (key, val) in params {
            if key != flagname {
                continue;
            }
            let bare_val = val.trim();
            if !bare_val.is_empty() {
                return Some(bare_val.to_string());
            }
        }
        None
    }

    /// Get platform/OEM value from cmdline file.
    pub fn get_platform() -> Result<String> {
        let content = std::fs::read_to_string(CMDLINE_PATH)?;

        match find_flag_value(CMDLINE_PLATFORM_FLAG, &content) {
            Some(platform) => Ok(platform),
            None => anyhow::bail!(
                "Couldn't find flag '{}' in cmdline file ({})",
                CMDLINE_PLATFORM_FLAG,
                CMDLINE_PATH
            ),
        }
    }
}

mod nvme {
    use super::*;

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct DevicesOutput {
        devices: Vec<Device>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "PascalCase")]
    pub(crate) struct Device {
        pub(crate) device_path: String,
        pub(crate) model_number: String,
        pub(crate) serial_number: String,
    }

    pub(crate) fn list() -> Result<Vec<Device>> {
        let o = Command::new("nvme")
            .args(&["list", "-o", "json"])
            .output()?;
        if !o.status.success() {
            bail!("Failed to list nvme devices");
        }
        let devs: DevicesOutput = serde_json::from_reader(&*o.stdout)?;
        Ok(devs.devices)
    }
}

mod lvm {
    use super::*;

    fn pvcreate(dev: &str) -> Result<()> {
        Command::new("lvm").arg("pvcreate").arg(dev).run()
    }

    fn escape(name: &str) -> String {
        name.replace('-', "--")
    }

    pub(crate) fn new_striped_lv(
        lvname: &str,
        vgname: &str,
        devices: &Vec<String>,
    ) -> Result<String> {
        for dev in devices {
            pvcreate(&dev)?;
        }
        Command::new("lvm")
            .arg("vgcreate")
            .arg(vgname)
            .args(devices)
            .run()?;
        Command::new("lvm")
            .arg("lvcreate")
            .args(&["--type", "striped", "--extents", "100%FREE"])
            .arg(vgname)
            .arg("--name")
            .arg(lvname)
            .run()?;
        Ok(format!("/dev/mapper/{}-{}", escape(vgname), escape(lvname)))
    }
}

mod aws {
    use super::*;

    const INSTANCE_MODEL: &str = "Amazon EC2 NVMe Instance Storage";

    pub(crate) fn devices() -> Result<Vec<String>> {
        Ok(nvme::list()?
            .into_iter()
            .filter_map(|dev| {
                if dev.model_number == INSTANCE_MODEL {
                    Some(dev.device_path)
                } else {
                    None
                }
            })
            .collect())
    }
}

// This one is totally made up for local testing; use e.g.
// cosa run --bind-ro /var/srv/walters/src/github/cgwalters/coreos-cloud-instance-store-provisioner/,/run/workdir -- -device nvme,drive=one,serial=CoreOSQEMUInstance1
// -drive if=none,id=one,file=/tmp/empty.qcow2  -device nvme,drive=two,serial=CoreOSQEMUInstance2 -drive if=none,id=two,file=/tmp/empty2.qcow2
mod qemu {
    use super::*;

    const PREFIX: &str = "CoreOSQEMUInstance";

    pub(crate) fn devices() -> Result<Vec<String>> {
        Ok(nvme::list()?
            .into_iter()
            .filter_map(|dev| {
                if dev.serial_number.starts_with(PREFIX) {
                    Some(dev.device_path)
                } else {
                    None
                }
            })
            .collect())
    }
}

mod systemd {
    use super::*;
    use libsystemd::unit;
    use std::io::Write as IoWrite;

    pub(crate) fn write_mount_unit(
        what_path: &str,
        where_path: &str,
        mnt_type: &str,
    ) -> Result<String> {
        let dir = openat::Dir::open("/etc/systemd/system")?;
        let name = format!("{}.mount", unit::escape_path(where_path));
        dir.write_file_with(&name, 0o644, |f| -> Result<()> {
            write!(
                f,
                r##"[Unit]
Before=local-fs.target

[Mount]
What={what_path}
Where={where_path}
Type={mnt_type}

[Install]
WantedBy=local-fs.target
"##,
                what_path = what_path,
                where_path = where_path,
                mnt_type = mnt_type
            )?;
            Ok(())
        })?;
        Ok(name)
    }
}

mod selinux {
    use super::*;

    pub(crate) fn copy_context<S: AsRef<Path>, D: AsRef<Path>>(src: S, dest: D) -> Result<()> {
        let src = src.as_ref();
        let dest = dest.as_ref();
        let mut refarg = std::ffi::OsString::from("--reference=");
        refarg.push(src);
        Command::new("chcon").arg(&refarg).arg(dest).run()?;
        Ok(())
    }
}

fn main() -> Result<()> {
    let configpath = Path::new(CONFIG_PATH);
    if !configpath.exists() {
        println!("No configuration specified.");
        return Ok(());
    }
    let config: Config =
        serde_yaml::from_reader(std::io::BufReader::new(std::fs::File::open(configpath)?))?;
    if config.directories.is_empty() {
        bail!("Specified directories list is empty");
    }
    let ephemeral = match coreos::get_platform()?.as_str() {
        "aws" => aws::devices()?,
        "qemu" => qemu::devices()?,
        other => {
            println!("Unhandled platform: {}", other);
            return Ok(());
        }
    };
    let dev = match ephemeral.len() {
        0 => {
            println!("No ephemeral devices found.");
            return Ok(());
        }
        1 => Cow::Borrowed(&ephemeral[0]),
        _ => Cow::Owned(lvm::new_striped_lv(
            "striped",
            "coreos-instance-vg",
            &ephemeral,
        )?),
    };
    let dev = dev.as_str();
    Command::new("mkfs.xfs").arg(dev).run()?;
    create_dir(MOUNTPOINT).context("creating mountpoint")?;
    let mountunit =
        systemd::write_mount_unit(dev, MOUNTPOINT, "xfs").context("failed to write mount unit")?;
    Command::new("systemctl").arg("daemon-reload").run()?;
    Command::new("systemctl")
        .args(&["enable", "--now"])
        .arg(&mountunit)
        .run()?;
    selinux::copy_context("/var", MOUNTPOINT)?;
    let root = openat::Dir::open("/").context("opening /")?;
    for d in config.directories.iter().map(Path::new) {
        let name = d
            .file_name()
            .ok_or_else(|| anyhow!("Expected filename in {:?}", d))?;
        let target = Path::new(MOUNTPOINT).join(name);
        create_dir(&target).context("creating target dir")?;
        if d.exists() {
            selinux::copy_context(&d, &target)?;
        }
        root.remove_all(d)
            .with_context(|| format!("Removing {:?}", d))?;
        std::os::unix::fs::symlink(&target, d)
            .with_context(|| format!("creating symlink {:?}", d))?;
        println!("Set up {:?} to use instance storage", d);
    }
    Ok(())
}
