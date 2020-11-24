//! Automatically set up a filesystem for instance-local storage
//! and redirect desired directory paths to it.  Good examples
//! for this are /var/lib/containers, /var/log, etc.
//! https://github.com/coreos/ignition/issues/1126

use anyhow::{anyhow, bail, Context, Result};
use openat_ext::OpenatDirExt;
use serde_derive::Deserialize;
use std::borrow::Cow;
use std::fs::create_dir;
use std::path::Path;
use std::process::Command;

const LABEL: &str = "ccisp-store";
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

mod block {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct DevicesOutput {
        blockdevices: Vec<Device>,
    }

    #[derive(Debug, Deserialize)]
    pub(crate) struct Device {
        pub(crate) name: String,
        pub(crate) serial: Option<String>,
        pub(crate) model: Option<String>,
        pub(crate) label: Option<String>,
        pub(crate) fstype: Option<String>,
        pub(crate) children: Option<Vec<Device>>,
    }

    impl Device {
        // RHEL8's lsblk doesn't have PATH, so we do it
        pub(crate) fn path(&self) -> String {
            format!("/dev/{}", &self.name)
        }
    }

    pub(crate) fn wipefs(dev: &str) -> Result<()> {
        Command::new("wipefs").arg("-a").arg(dev).run()?;
        Ok(())
    }

    pub(crate) fn list() -> Result<Vec<Device>> {
        let o = Command::new("lsblk")
            .args(&["-J", "-o", "NAME,SERIAL,MODEL,LABEL,FSTYPE"])
            .output()?;
        if !o.status.success() {
            bail!("Failed to list block devices");
        }
        let devs: DevicesOutput = serde_json::from_reader(&*o.stdout)?;
        Ok(devs.blockdevices)
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
        devices: &[String],
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
        Ok(block::list()?
            .into_iter()
            .filter(|dev| {
                dev.model
                    .as_ref()
                    .filter(|model| model.trim() == INSTANCE_MODEL)
                    .is_some()
            })
            .map(|d| d.path())
            .collect())
    }
}

mod azure {
    use super::*;
    use block::Device;

    const MODEL: &str = "Virtual Disk";
    const FSTYPE: &str = "ntfs";
    const LABEL: &str = "Temporary Storage";

    /// On Azure, we the device will be pre-formatted as ntfs, so we actually
    /// look for a block device with a single child that matches.
    fn filtermap_child_ntfs(dev: Device) -> Option<String> {
        let child = if let Some(children) = dev.children.as_ref() {
            if children.len() == 1 {
                &children[0]
            } else {
                return None;
            }
        } else {
            return None;
        };
        if let (Some(label), Some(fstype)) = (child.label.as_ref(), child.fstype.as_ref()) {
            if label.as_str().trim() == LABEL && fstype.as_str().trim() == FSTYPE {
                let devpath = dev.path();
                return Some(devpath);
            }
        }
        None
    }

    pub(crate) fn devices() -> Result<Vec<String>> {
        let r: Result<Vec<String>> = block::list()?
            .into_iter()
            .filter(|dev| {
                dev.model
                    .as_ref()
                    .filter(|m| m.as_str().trim() == MODEL)
                    .is_some()
            })
            .filter_map(filtermap_child_ntfs)
            .map(|dev: String| {
                // Azure helpfully sets it up as NTFS,
                // so we need to wipe that.
                block::wipefs(&dev)?;
                Ok(dev)
            })
            .collect();
        Ok(r?)
    }
}

// This one is totally made up for local testing; use e.g.
mod qemu {
    use super::*;

    const PREFIX: &str = "CoreOSQEMUInstance";

    pub(crate) fn devices() -> Result<Vec<String>> {
        Ok(block::list()?
            .into_iter()
            .filter(|dev| {
                dev.serial
                    .as_ref()
                    .filter(|serial| serial.trim().starts_with(PREFIX))
                    .is_some()
            })
            .map(|dev| dev.path())
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
        opts: Option<&str>,
    ) -> Result<String> {
        let dir = openat::Dir::open("/etc/systemd/system")?;
        let name = format!("{}.mount", unit::escape_path(where_path));
        let opts = opts
            .map(|opts| Cow::Owned(format!("Options={}", opts)))
            .unwrap_or_else(|| Cow::Borrowed(""));
        dir.write_file_with(&name, 0o644, |f| -> Result<()> {
            write!(
                f,
                r##"[Unit]
Before=local-fs.target
RequiresMountsFor={what_path}

[Mount]
What={what_path}
Where={where_path}
Type={mnt_type}
{opts}

[Install]
WantedBy=local-fs.target
"##,
                what_path = what_path,
                where_path = where_path,
                mnt_type = mnt_type,
                opts = opts,
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

    // Find all instance-local devices
    let instance_devs = match coreos::get_platform()?.as_str() {
        "aws" => aws::devices()?,
        "azure" => azure::devices()?,
        "qemu" => qemu::devices()?,
        other => {
            println!("Unhandled platform: {}", other);
            return Ok(());
        }
    };

    // Discover all instance-local block devices
    let dev = match instance_devs.len() {
        // Not finding any devices isn't currently an error; we want to
        // support being run from instance types that don't have any
        // allocated.
        0 => {
            println!("No ephemeral devices found.");
            return Ok(());
        }
        // If there's just one block device, we use it directly
        1 => Cow::Borrowed(&instance_devs[0]),
        // If there are more than one, we default to creating a striped LVM volume
        // across them.
        _ => Cow::Owned(lvm::new_striped_lv(
            "striped",
            "coreos-instance-vg",
            &instance_devs,
        )?),
    };
    let dev = dev.as_str();

    // Format as XFS
    Command::new("mkfs.xfs")
        .args(&["-L", LABEL])
        .arg(dev)
        .run()?;

    // Create the mountpoint and mount unit, and mount it
    create_dir(MOUNTPOINT).context("creating mountpoint")?;
    let dev = format!("/dev/disk/by-label/{}", LABEL);
    let mountunit = systemd::write_mount_unit(&dev, MOUNTPOINT, "xfs", None)
        .context("failed to write mount unit")?;
    Command::new("systemctl").arg("daemon-reload").run()?;
    Command::new("systemctl")
        .args(&["enable", "--now"])
        .arg(&mountunit)
        .run()?;
    // We need to ensure it has a SELinux label.
    selinux::copy_context("/var", MOUNTPOINT)?;

    // Iterate over the desired directories (should be under /var)
    // that we want to have mounted instance-local.  Software
    // using these directories should ideally be prepared to start
    // with it empty.
    let root = openat::Dir::open("/").context("opening /")?;
    let mut units = Vec::new();
    for d in config.directories.iter().map(Path::new) {
        let d_utf8 = d.to_str().expect("utf8");
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
        std::fs::create_dir(d).with_context(|| format!("Creating {}", d_utf8))?;
        // Sadly crio on RHEL8 at least bails out if /var/lib/containers is a symlink.
        // So we use bind mounts instead.
        units.push(systemd::write_mount_unit(
            target.to_str().expect("utf8"),
            d_utf8,
            "none",
            Some("bind"),
        )?);
        println!("Set up {:?} to use instance storage", d);
    }
    // Enable+start all the mount units we set up
    Command::new("systemctl").arg("daemon-reload").run()?;
    for unit in units {
        Command::new("systemctl")
            .args(&["enable", "--now"])
            .arg(&unit)
            .run()?;
    }
    Ok(())
}
