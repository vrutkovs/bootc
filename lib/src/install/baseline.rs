//! # The baseline installer
//!
//! This module handles creation of simple root filesystem setups.  At the current time
//! it's very simple - just a direct filesystem (e.g. xfs, ext4, btrfs etc.).  It is
//! intended to add opinionated handling of TPM2-bound LUKS too.  But that's about it;
//! other more complex flows should set things up externally and use `bootc install-to-filesystem`.

use std::borrow::Cow;
use std::fmt::Display;
use std::process::Command;
use std::process::Stdio;

use anyhow::Ok;
use anyhow::{Context, Result};
use camino::Utf8Path;
use camino::Utf8PathBuf;
use cap_std::fs::Dir;
use cap_std_ext::cap_std;
use clap::ArgEnum;
use fn_error_context::context;
use serde::{Deserialize, Serialize};

use super::MountSpec;
use super::RootSetup;
use super::RUN_BOOTC;
use super::RW_KARG;
use crate::lsm::lsm_label;
use crate::mount;
use crate::task::Task;

pub(crate) const BOOTPN: u32 = 3;
// This ensures we end up under 512 to be small-sized.
pub(crate) const BOOTPN_SIZE_MB: u32 = 510;
pub(crate) const ROOTPN: u32 = 4;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub(crate) const EFIPN: u32 = 2;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub(crate) const EFIPN_SIZE_MB: u32 = 512;
#[cfg(target_arch = "aarch64")]
pub(crate) const RESERVEDPN: u32 = 1;
#[cfg(target_arch = "ppc64")]
pub(crate) const PREPPN: u32 = 1;
#[cfg(target_arch = "ppc64")]
pub(crate) const RESERVEDPN: u32 = 1;

#[derive(clap::ValueEnum, Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Filesystem {
    Xfs,
    Ext4,
    Btrfs,
}

impl Default for Filesystem {
    fn default() -> Self {
        // Obviously this should be configurable.
        Self::Xfs
    }
}

impl Display for Filesystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.to_possible_value().unwrap().get_name().fmt(f)
    }
}

#[derive(clap::ValueEnum, Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BlockSetup {
    Direct,
    Tpm2Luks,
}

impl Default for BlockSetup {
    fn default() -> Self {
        Self::Direct
    }
}

/// Options for installing to a block device
#[derive(Debug, Clone, clap::Args, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct InstallBlockDeviceOpts {
    /// Target block device for installation.  The entire device will be wiped.
    pub(crate) device: Utf8PathBuf,

    /// Automatically wipe all existing data on device
    #[clap(long)]
    #[serde(default)]
    pub(crate) wipe: bool,

    /// Target root block device setup.
    ///
    /// direct: Filesystem written directly to block device
    /// tpm2-luks: Bind unlock of filesystem to presence of the default tpm2 device.
    #[clap(long, value_enum, default_value_t)]
    #[serde(default)]
    pub(crate) block_setup: BlockSetup,

    /// Target root filesystem type.
    #[clap(long, value_enum, default_value_t)]
    #[serde(default)]
    pub(crate) filesystem: Filesystem,

    /// Size of the root partition (default specifier: M).  Allowed specifiers: M (mebibytes), G (gibibytes), T (tebibytes).
    ///
    /// By default, all remaining space on the disk will be used.
    #[clap(long)]
    pub(crate) root_size: Option<String>,
}

fn sgdisk_partition(
    sgdisk: &mut Command,
    n: u32,
    part: impl AsRef<str>,
    name: impl AsRef<str>,
    typecode: Option<&str>,
) {
    sgdisk.arg("-n");
    sgdisk.arg(format!("{n}:{}", part.as_ref()));
    sgdisk.arg("-c");
    sgdisk.arg(format!("{n}:{}", name.as_ref()));
    if let Some(typecode) = typecode {
        sgdisk.arg("-t");
        sgdisk.arg(format!("{n}:{typecode}"));
    }
}

fn mkfs<'a>(
    dev: &str,
    fs: Filesystem,
    label: Option<&'_ str>,
    opts: impl IntoIterator<Item = &'a str>,
) -> Result<uuid::Uuid> {
    let u = uuid::Uuid::new_v4();
    let mut t = Task::new("Creating filesystem", format!("mkfs.{fs}"));
    match fs {
        Filesystem::Xfs => {
            t.cmd.arg("-m");
            t.cmd.arg(format!("uuid={u}"));
        }
        Filesystem::Btrfs | Filesystem::Ext4 => {
            t.cmd.arg("-U");
            t.cmd.arg(u.to_string());
        }
    };
    // Today all the above mkfs commands take -L
    if let Some(label) = label {
        t.cmd.args(["-L", label]);
    }
    t.cmd.args(opts);
    t.cmd.arg(dev);
    // All the mkfs commands are unnecessarily noisy by default
    t.cmd.stdout(Stdio::null());
    t.run()?;
    Ok(u)
}

#[context("Creating rootfs")]
pub(crate) fn install_create_rootfs(opts: InstallBlockDeviceOpts) -> Result<RootSetup> {
    // Verify that the target is empty (if not already wiped in particular, but it's
    // also good to verify that the wipe worked)
    let device = crate::blockdev::list_dev(&opts.device)?;

    // Handle wiping any existing data
    if opts.wipe {
        let dev = &opts.device;
        for child in device.children.iter().flatten() {
            let child = child.path();
            println!("Wiping {child}");
            crate::blockdev::wipefs(Utf8Path::new(&child))?;
        }
        println!("Wiping {dev}");
        crate::blockdev::wipefs(dev)?;
    } else if device.has_children() {
        anyhow::bail!(
            "Detected existing partitions on {}; use e.g. `wipefs` if you intend to overwrite",
            opts.device
        );
    }

    let run_bootc = Utf8Path::new(RUN_BOOTC);
    let mntdir = run_bootc.join("mounts");
    if mntdir.exists() {
        std::fs::remove_dir_all(&mntdir)?;
    }
    let devdir = mntdir.join("dev");
    std::fs::create_dir_all(&devdir)?;
    Task::new_and_run(
        "Mounting devtmpfs",
        "mount",
        ["devtmpfs", "-t", "devtmpfs", devdir.as_str()],
    )?;

    // Now at this point, our /dev is a stale snapshot because we don't have udev running.
    // So from hereon after, we prefix devices with our temporary devtmpfs mount.
    let reldevice = opts
        .device
        .strip_prefix("/dev/")
        .context("Absolute device path in /dev/ required")?;
    let device = devdir.join(reldevice);

    let root_size = opts
        .root_size
        .as_deref()
        .map(crate::blockdev::parse_size_mib)
        .transpose()
        .context("Parsing root size")?;

    // Create a temporary directory to use for mount points.  Note that we're
    // in a mount namespace, so these should not be visible on the host.
    let rootfs = mntdir.join("rootfs");
    std::fs::create_dir_all(&rootfs)?;
    let bootfs = mntdir.join("boot");
    std::fs::create_dir_all(bootfs)?;

    // Run sgdisk to create partitions.
    let mut sgdisk = Task::new("Initializing partitions", "sgdisk");
    // sgdisk is too verbose
    sgdisk.cmd.stdout(Stdio::null());
    sgdisk.cmd.arg("-Z");
    sgdisk.cmd.arg(&device);
    sgdisk.cmd.args(["-U", "R"]);
    #[allow(unused_assignments)]
    if cfg!(target_arch = "x86_64") {
        // BIOS-BOOT
        sgdisk_partition(
            &mut sgdisk.cmd,
            1,
            "0:+1M",
            "BIOS-BOOT",
            Some("21686148-6449-6E6F-744E-656564454649"),
        );
    } else if cfg!(target_arch = "aarch64") {
        // reserved
        sgdisk_partition(
            &mut sgdisk.cmd,
            1,
            "0:+1M",
            "reserved",
            Some("8DA63339-0007-60C0-C436-083AC8230908"),
        );
    } else {
        anyhow::bail!("Unsupported architecture: {}", std::env::consts::ARCH);
    }

    let espdev = if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
        sgdisk_partition(
            &mut sgdisk.cmd,
            EFIPN,
            format!("0:+{EFIPN_SIZE_MB}M"),
            "EFI-SYSTEM",
            Some("C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
        );
        Some(format!("{device}{EFIPN}"))
    } else {
        None
    };

    sgdisk_partition(
        &mut sgdisk.cmd,
        BOOTPN,
        format!("0:+{BOOTPN_SIZE_MB}M"),
        "boot",
        None,
    );
    let root_size = root_size
        .map(|v| Cow::Owned(format!("0:{v}M")))
        .unwrap_or_else(|| Cow::Borrowed("0:0"));
    sgdisk_partition(
        &mut sgdisk.cmd,
        ROOTPN,
        root_size,
        "root",
        Some("0FC63DAF-8483-4772-8E79-3D69D8477DE4"),
    );
    sgdisk.run()?;

    // Reread the partition table
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&device)
            .with_context(|| format!("opening {device}"))?;
        crate::blockdev::reread_partition_table(&mut f, true)
            .context("Rereading partition table")?;
    }

    crate::blockdev::udev_settle()?;

    match opts.block_setup {
        BlockSetup::Direct => {}
        // TODO
        BlockSetup::Tpm2Luks => anyhow::bail!("tpm2-luks is not implemented yet"),
    }

    // TODO: make this configurable
    let bootfs_type = Filesystem::Ext4;

    // Initialize the /boot filesystem
    let bootdev = &format!("{device}{BOOTPN}");
    let boot_uuid = mkfs(bootdev, bootfs_type, Some("boot"), []).context("Initializing /boot")?;

    // Initialize rootfs
    let rootdev = &format!("{device}{ROOTPN}");
    let root_uuid = mkfs(rootdev, opts.filesystem, Some("root"), [])?;
    let rootarg = format!("root=UUID={root_uuid}");
    let bootsrc = format!("UUID={boot_uuid}");
    let bootarg = format!("boot={bootsrc}");
    let boot = MountSpec::new(bootsrc.as_str(), "/boot");
    let kargs = vec![rootarg, RW_KARG.to_string(), bootarg];

    mount::mount(rootdev, &rootfs)?;
    lsm_label(&rootfs, "/".into(), false)?;
    let rootfs_fd = Dir::open_ambient_dir(&rootfs, cap_std::ambient_authority())?;
    let bootfs = rootfs.join("boot");
    std::fs::create_dir(&bootfs).context("Creating /boot")?;
    // The underlying directory on the root should be labeled
    lsm_label(&bootfs, "/boot".into(), false)?;
    mount::mount(bootdev, &bootfs)?;
    // And we want to label the root mount of /boot
    lsm_label(&bootfs, "/boot".into(), false)?;

    // Create the EFI system partition, if applicable
    if let Some(espdev) = espdev {
        Task::new("Creating ESP filesystem", "mkfs.fat")
            .args([espdev.as_str(), "-n", "EFI-SYSTEM"])
            .quiet_output()
            .run()?;
        let efifs_path = bootfs.join(crate::bootloader::EFI_DIR);
        std::fs::create_dir(&efifs_path).context("Creating efi dir")?;
        mount::mount(&espdev, &efifs_path)?;
    }

    Ok(RootSetup {
        device,
        rootfs,
        rootfs_fd,
        boot,
        kargs,
    })
}
