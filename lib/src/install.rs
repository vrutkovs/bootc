//! # Writing a container to a block device in a bootable way
//!
//! This module supports installing a bootc-compatible image to
//! a block device directly via the `install` verb, or to an externally
//! set up filesystem via `install-to-filesystem`.

// This sub-module is the "basic" installer that handles creating basic block device
// and filesystem setup.
mod baseline;

use std::io::BufWriter;
use std::io::Write;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Ok;
use anyhow::{anyhow, Context, Result};
use camino::Utf8Path;
use camino::Utf8PathBuf;
use cap_std::fs::Dir;
use cap_std_ext::cap_std;
use cap_std_ext::prelude::CapStdExtDirExt;
use cap_std_ext::rustix::fs::MetadataExt;

use fn_error_context::context;
use ostree::gio;
use ostree_ext::container as ostree_container;
use ostree_ext::container::SignatureSource;
use ostree_ext::ostree;
use ostree_ext::prelude::Cast;
use serde::{Deserialize, Serialize};

use self::baseline::InstallBlockDeviceOpts;
use crate::lsm::lsm_label;
use crate::task::Task;
use crate::utils::run_in_host_mountns;

/// The default "stateroot" or "osname"; see https://github.com/ostreedev/ostree/issues/2794
const STATEROOT_DEFAULT: &str = "default";
/// The toplevel boot directory
const BOOT: &str = "boot";
/// Directory for transient runtime state
const RUN_BOOTC: &str = "/run/bootc";
/// This is an ext4 special directory we need to ignore.
const LOST_AND_FOUND: &str = "lost+found";

/// Kernel argument used to specify we want the rootfs mounted read-write by default
const RW_KARG: &str = "rw";

#[derive(clap::Args, Debug, Clone, Serialize, Deserialize)]
pub(crate) struct InstallTargetOpts {
    // TODO: A size specifier which allocates free space for the root in *addition* to the base container image size
    // pub(crate) root_additional_size: Option<String>
    /// The transport; e.g. oci, oci-archive.  Defaults to `registry`.
    #[clap(long, default_value = "registry")]
    #[serde(default)]
    pub(crate) target_transport: String,

    /// Specify the image to fetch for subsequent updates
    #[clap(long)]
    pub(crate) target_imgref: Option<String>,

    /// Explicitly opt-out of requiring any form of signature verification.
    #[clap(long)]
    #[serde(default)]
    pub(crate) target_no_signature_verification: bool,

    /// Enable verification via an ostree remote
    #[clap(long)]
    pub(crate) target_ostree_remote: Option<String>,
}

#[derive(clap::Args, Debug, Clone, Serialize, Deserialize)]
pub(crate) struct InstallConfigOpts {
    /// Path to an Ignition config file
    #[clap(long, value_parser)]
    pub(crate) ignition_file: Option<Utf8PathBuf>,

    /// Digest (type-value) of the Ignition config
    ///
    /// Verify that the Ignition config matches the specified digest,
    /// formatted as <type>-<hexvalue>.  <type> can be sha256 or sha512.
    #[clap(long, value_name = "digest", value_parser)]
    pub(crate) ignition_hash: Option<crate::ignition::IgnitionHash>,

    /// Disable SELinux in the target (installed) system.
    ///
    /// This is currently necessary to install *from* a system with SELinux disabled
    /// but where the target does have SELinux enabled.
    #[clap(long)]
    #[serde(default)]
    pub(crate) disable_selinux: bool,

    // Only occupy at most this much space (if no units are provided, GB is assumed).
    // Using this option reserves space for partitions created dynamically on the
    // next boot, or by subsequent tools.
    //    pub(crate) size: Option<String>,
    #[clap(long)]
    /// Add a kernel argument
    karg: Option<Vec<String>>,
}

/// Perform an installation to a block device.
#[derive(Debug, Clone, clap::Parser, Serialize, Deserialize)]
pub(crate) struct InstallOpts {
    #[clap(flatten)]
    #[serde(flatten)]
    pub(crate) block_opts: InstallBlockDeviceOpts,

    #[clap(flatten)]
    #[serde(flatten)]
    pub(crate) target_opts: InstallTargetOpts,

    #[clap(flatten)]
    #[serde(flatten)]
    pub(crate) config_opts: InstallConfigOpts,
}

/// Options for installing to a filesystem
#[derive(Debug, Clone, clap::Args)]
pub(crate) struct InstallTargetFilesystemOpts {
    /// Path to the mounted root filesystem.
    ///
    /// By default, the filesystem UUID will be discovered and used for mounting.
    /// To override this, use `--root-mount-spec`.
    pub(crate) root_path: Utf8PathBuf,

    /// Source device specification for the root filesystem.  For example, UUID=2e9f4241-229b-4202-8429-62d2302382e1
    #[clap(long)]
    pub(crate) root_mount_spec: Option<String>,

    /// Comma-separated mount options for the root filesystem.  For example: rw,prjquota
    #[clap(long)]
    pub(crate) root_options: Option<String>,

    /// Mount specification for the /boot filesystem.
    ///
    /// At the current time, a separate /boot is required.  This restriction will be lifted in
    /// future versions.  If not specified, the filesystem UUID will be used.
    #[clap(long)]
    pub(crate) boot_mount_spec: Option<String>,

    /// Automatically wipe existing data on the filesystems.
    #[clap(long)]
    pub(crate) wipe: bool,
}

/// Perform an installation to a mounted filesystem.
#[derive(Debug, Clone, clap::Parser)]
pub(crate) struct InstallToFilesystemOpts {
    #[clap(flatten)]
    pub(crate) filesystem_opts: InstallTargetFilesystemOpts,

    #[clap(flatten)]
    pub(crate) target_opts: InstallTargetOpts,

    #[clap(flatten)]
    pub(crate) config_opts: InstallConfigOpts,
}

// Shared read-only global state
pub(crate) struct State {
    /// Image reference we'll pull from (today always containers-storage: type)
    source_imageref: ostree_container::ImageReference,
    /// The digest to use for pulls
    source_digest: String,
    /// Force SELinux off in target system
    override_disable_selinux: bool,
    config_opts: InstallConfigOpts,
    target_opts: InstallTargetOpts,
}

/// Path to initially deployed version information
const BOOTC_ALEPH_PATH: &str = ".bootc-aleph.json";

/// The "aleph" version information is injected into /root/.bootc-aleph.json
/// and contains the image ID that was initially used to install.  This can
/// be used to trace things like the specific version of `mkfs.ext4` or
/// kernel version that was used.
#[derive(Debug, Serialize)]
struct InstallAleph {
    /// Digested pull spec for installed image
    image: String,
    kernel: String,
}

/// A mount specification is a subset of a line in `/etc/fstab`.
///
/// There are 3 (ASCII) whitespace separated values:
///
/// SOURCE TARGET [OPTIONS]
///
/// Examples:
///   - /dev/vda3 /boot ext4 ro
///   - /dev/nvme0n1p4 /
///   - /dev/sda2 /var/mnt xfs
#[derive(Debug, Clone)]
pub(crate) struct MountSpec {
    pub(crate) source: String,
    pub(crate) target: String,
    pub(crate) fstype: String,
    pub(crate) options: Option<String>,
}

impl MountSpec {
    const AUTO: &'static str = "auto";

    pub(crate) fn new(src: &str, target: &str) -> Self {
        MountSpec {
            source: src.to_string(),
            target: target.to_string(),
            fstype: Self::AUTO.to_string(),
            options: None,
        }
    }

    /// Construct a new mount that uses the provided uuid as a source.
    pub(crate) fn new_uuid_src(uuid: &str, target: &str) -> Self {
        Self::new(&format!("UUID={uuid}"), target)
    }

    pub(crate) fn get_source_uuid(&self) -> Option<&str> {
        if let Some((t, rest)) = self.source.split_once('=') {
            if t.eq_ignore_ascii_case("uuid") {
                return Some(rest);
            }
        }
        None
    }

    pub(crate) fn to_fstab(&self) -> String {
        let options = self.options.as_deref().unwrap_or("defaults");
        format!(
            "{} {} {} {} 0 0",
            self.source, self.target, self.fstype, options
        )
    }
}

impl FromStr for MountSpec {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let mut parts = s.split_ascii_whitespace().fuse();
        let source = parts.next().unwrap_or_default();
        if source.is_empty() {
            anyhow::bail!("Invalid empty mount specification");
        }
        let target = parts
            .next()
            .ok_or_else(|| anyhow!("Missing target in mount specification {s}"))?;
        let fstype = parts.next().unwrap_or(Self::AUTO);
        let options = parts.next().map(ToOwned::to_owned);
        Ok(Self {
            source: source.to_string(),
            fstype: fstype.to_string(),
            target: target.to_string(),
            options,
        })
    }
}

fn bind_mount_from_host(src: impl AsRef<Utf8Path>, dest: impl AsRef<Utf8Path>) -> Result<()> {
    let src = src.as_ref();
    let dest = dest.as_ref();
    tracing::debug!("Mounting host {src} to {dest}");
    std::fs::create_dir_all(dest).with_context(|| format!("Creating {dest}"))?;
    // Here's the magic trick; modern versions of the `mount` command support a `-N` argument
    // to perform the mount in a distinct target namespace.  But, what we want to is the inverse
    // of this - we want to grab a host/root filesystem mount point.  So we explicitly enter
    // the host's mount namespace, then give `mount` our own pid (from which it finds the mount namespace).
    let desc = format!("Bind mounting {src} from host");
    let target = format!("{}", nix::unistd::getpid());
    Task::new_cmd(desc, run_in_host_mountns("mount"))
        .quiet()
        .args(["--bind", "-N", target.as_str(), src.as_str(), dest.as_str()])
        .run()
}

#[context("Creating ostree deployment")]
async fn initialize_ostree_root_from_self(
    state: &State,
    root_setup: &RootSetup,
) -> Result<InstallAleph> {
    let rootfs_dir = &root_setup.rootfs_fd;
    let rootfs = root_setup.rootfs.as_path();
    let opts = &state.target_opts;
    let cancellable = gio::Cancellable::NONE;

    // Parse the target CLI image reference options
    let target_sigverify = if opts.target_no_signature_verification {
        SignatureSource::ContainerPolicyAllowInsecure
    } else if let Some(remote) = opts.target_ostree_remote.as_deref() {
        SignatureSource::OstreeRemote(remote.to_string())
    } else {
        SignatureSource::ContainerPolicy
    };
    let target_imgref = if let Some(imgref) = opts.target_imgref.as_ref() {
        let transport = ostree_container::Transport::try_from(opts.target_transport.as_str())?;
        let imgref = ostree_container::ImageReference {
            transport,
            name: imgref.to_string(),
        };
        ostree_container::OstreeImageReference {
            sigverify: target_sigverify,
            imgref,
        }
    } else {
        ostree_container::OstreeImageReference {
            sigverify: target_sigverify,
            imgref: state.source_imageref.clone(),
        }
    };

    // TODO: make configurable?
    let stateroot = STATEROOT_DEFAULT;
    Task::new_and_run(
        "Initializing ostree layout",
        "ostree",
        ["admin", "init-fs", "--modern", rootfs.as_str()],
    )?;

    for (k, v) in [("sysroot.bootloader", "none"), ("sysroot.readonly", "true")] {
        Task::new("Configuring ostree repo", "ostree")
            .args(["config", "--repo", "ostree/repo", "set", k, v])
            .cwd(rootfs_dir)?
            .quiet()
            .run()?;
    }
    Task::new("Initializing sysroot", "ostree")
        .args(["admin", "os-init", stateroot, "--sysroot", "."])
        .cwd(rootfs_dir)?
        .run()?;

    // Ensure everything in the ostree repo is labeled
    lsm_label(&rootfs.join("ostree"), "/usr".into(), true)?;

    let sysroot = ostree::Sysroot::new(Some(&gio::File::for_path(rootfs)));
    sysroot.load(cancellable)?;

    // We need to fetch the container image from the root mount namespace
    let skopeo_cmd = run_in_host_mountns("skopeo");
    let proxy_cfg = ostree_container::store::ImageProxyConfig {
        skopeo_cmd: Some(skopeo_cmd),
        ..Default::default()
    };

    let mut temporary_dir = None;
    let src_imageref = if skopeo_supports_containers_storage()? {
        // We always use exactly the digest of the running image to ensure predictability.
        let spec =
            crate::utils::digested_pullspec(&state.source_imageref.name, &state.source_digest);
        ostree_container::ImageReference {
            transport: ostree_container::Transport::ContainerStorage,
            name: spec,
        }
    } else {
        let td = tempfile::tempdir_in("/var/tmp")?;
        let path: &Utf8Path = td.path().try_into().unwrap();
        let r = copy_to_oci(&state.source_imageref, path)?;
        temporary_dir = Some(td);
        r
    };
    let src_imageref = ostree_container::OstreeImageReference {
        // There are no signatures to verify since we're fetching the already
        // pulled container.
        sigverify: ostree_container::SignatureSource::ContainerPolicyAllowInsecure,
        imgref: src_imageref,
    };

    let kargs = root_setup
        .kargs
        .iter()
        .map(|v| v.as_str())
        .collect::<Vec<_>>();
    #[allow(clippy::needless_update)]
    let options = ostree_container::deploy::DeployOpts {
        kargs: Some(kargs.as_slice()),
        target_imgref: Some(&target_imgref),
        proxy_cfg: Some(proxy_cfg),
        ..Default::default()
    };
    println!("Creating initial deployment");
    let state =
        ostree_container::deploy::deploy(&sysroot, stateroot, &src_imageref, Some(options)).await?;
    let target_image = target_imgref.to_string();
    let digest = state.manifest_digest;
    println!("Installed: {target_image}");
    println!("   Digest: {digest}");

    drop(temporary_dir);

    // Write the entry for /boot to /etc/fstab.  TODO: Encourage OSes to use the karg?
    // Or better bind this with the grub data.
    sysroot.load(cancellable)?;
    let deployment = sysroot
        .deployments()
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Failed to find deployment"))?;
    // SAFETY: There must be a path
    let path = sysroot.deployment_dirpath(&deployment).unwrap();
    let root = rootfs_dir
        .open_dir(path.as_str())
        .context("Opening deployment dir")?;
    let mut f = {
        let mut opts = cap_std::fs::OpenOptions::new();
        root.open_with("etc/fstab", opts.append(true).write(true).create(true))
            .context("Opening etc/fstab")
            .map(BufWriter::new)?
    };
    writeln!(f, "{}", root_setup.boot.to_fstab())?;
    f.flush()?;

    let uname = cap_std_ext::rustix::process::uname();

    let aleph = InstallAleph {
        image: src_imageref.imgref.name.clone(),
        kernel: uname.release().to_str()?.to_string(),
    };

    Ok(aleph)
}

#[context("Copying to oci")]
fn copy_to_oci(
    src_imageref: &ostree_container::ImageReference,
    dir: &Utf8Path,
) -> Result<ostree_container::ImageReference> {
    tracing::debug!("Copying {src_imageref}");
    let src_imageref = src_imageref.to_string();
    let dest_imageref = ostree_container::ImageReference {
        transport: ostree_container::Transport::OciDir,
        name: dir.to_string(),
    };
    let dest_imageref_str = dest_imageref.to_string();
    Task::new_cmd(
        "Copying to temporary OCI (skopeo is too old)",
        run_in_host_mountns("skopeo"),
    )
    .args([
        "copy",
        // TODO: enable this once ostree is fixed "--dest-oci-accept-uncompressed-layers",
        src_imageref.as_str(),
        dest_imageref_str.as_str(),
    ])
    .run()?;
    Ok(dest_imageref)
}

#[context("Querying skopeo version")]
fn skopeo_supports_containers_storage() -> Result<bool> {
    let o = run_in_host_mountns("skopeo").arg("--version").output()?;
    let st = o.status;
    if !st.success() {
        anyhow::bail!("Failed to run skopeo --version: {st:?}");
    }
    let stdout = String::from_utf8(o.stdout).context("Parsing skopeo version")?;
    let mut v = stdout
        .strip_prefix("skopeo version ")
        .map(|v| v.split('.'))
        .ok_or_else(|| anyhow::anyhow!("Unexpected output from skopeo version"))?;
    let major = v
        .next()
        .ok_or_else(|| anyhow::anyhow!("Missing major version"))?;
    let minor = v
        .next()
        .ok_or_else(|| anyhow::anyhow!("Missing minor version"))?;
    let (major, minor) = (major.parse::<u64>()?, minor.parse::<u64>()?);
    Ok(major > 1 || minor > 10)
}

pub(crate) struct RootSetup {
    device: Utf8PathBuf,
    rootfs: Utf8PathBuf,
    rootfs_fd: Dir,
    boot: MountSpec,
    kargs: Vec<String>,
}

fn require_boot_uuid(spec: &MountSpec) -> Result<&str> {
    spec.get_source_uuid()
        .ok_or_else(|| anyhow!("/boot is not specified via UUID= (this is currently required)"))
}

impl RootSetup {
    /// Get the UUID= mount specifier for the /boot filesystem.  At the current time this is
    /// required.
    fn get_boot_uuid(&self) -> Result<&str> {
        require_boot_uuid(&self.boot)
    }
}

pub(crate) struct SourceData {
    /// The embedded base OSTree commit checksum
    #[allow(dead_code)]
    pub(crate) commit: String,
    /// Whether or not SELinux appears to be enabled in the source commit
    pub(crate) selinux: bool,
}

#[context("Gathering source data")]
fn gather_source_data() -> Result<SourceData> {
    let cancellable = ostree::gio::Cancellable::NONE;
    let commit = Task::new("Reading ostree commit", "ostree")
        .args(["--repo=/ostree/repo", "rev-parse", "--single"])
        .quiet()
        .read()?;
    let root = cap_std::fs::Dir::open_ambient_dir("/", cap_std::ambient_authority())?;
    let repo = ostree::Repo::open_at_dir(&root, "ostree/repo")?;
    let root = repo
        .read_commit(commit.trim(), cancellable)
        .context("Reading commit")?
        .0;
    let root = root.downcast_ref::<ostree::RepoFile>().unwrap();
    let xattrs = root.xattrs(cancellable)?;
    let selinux = crate::lsm::xattrs_have_selinux(&xattrs);
    Ok(SourceData { commit, selinux })
}

/// If we detect that the target ostree commit has SELinux labels,
/// and we aren't passed an override to disable it, then ensure
/// the running process is labeled with install_t so it can
/// write arbitrary labels.
pub(crate) fn reexecute_self_for_selinux_if_needed(
    srcdata: &SourceData,
    override_disable_selinux: bool,
) -> Result<bool> {
    let mut ret_did_override = false;
    // If the target state has SELinux enabled, we need to check the host state.
    if srcdata.selinux {
        let host_selinux = crate::lsm::selinux_enabled()?;
        tracing::debug!("Target has SELinux, host={host_selinux}");
        if host_selinux {
            // /sys/fs/selinuxfs is not normally mounted, so we do that now.
            // Because SELinux enablement status is cached process-wide and was very likely
            // already queried by something else (e.g. glib's constructor), we would also need
            // to re-exec.  But, selinux_ensure_install does that unconditionally right now too,
            // so let's just fall through to that.
            crate::lsm::container_setup_selinux()?;
            // This will re-execute the current process (once).
            crate::lsm::selinux_ensure_install()?;
        } else if override_disable_selinux {
            ret_did_override = true;
            println!("notice: Target has SELinux enabled, overriding to disable")
        } else {
            anyhow::bail!(
                "Host kernel does not have SELinux support, but target enables it by default"
            );
        }
    } else {
        tracing::debug!("Target does not enable SELinux");
    }
    Ok(ret_did_override)
}

/// Trim, flush outstanding writes, and freeze/thaw the target mounted filesystem;
/// these steps prepare the filesystem for its first booted use.
pub(crate) fn finalize_filesystem(fs: &Utf8Path) -> Result<()> {
    let fsname = fs.file_name().unwrap();
    // fstrim ensures the underlying block device knows about unused space
    Task::new_and_run(format!("Trimming {fsname}"), "fstrim", ["-v", fs.as_str()])?;
    // Remounting readonly will flush outstanding writes and ensure we error out if there were background
    // writeback problems.
    Task::new(format!("Finalizing filesystem {fsname}"), "mount")
        .args(["-o", "remount,ro", fs.as_str()])
        .run()?;
    // Finally, freezing (and thawing) the filesystem will flush the journal, which means the next boot is clean.
    for a in ["-f", "-u"] {
        Task::new("Flushing filesystem journal", "xfs_freeze")
            .quiet()
            .args([a, fs.as_str()])
            .run()?;
    }
    Ok(())
}

/// Preparation for an install; validates and prepares some (thereafter immutable) global state.
async fn prepare_install(
    config_opts: InstallConfigOpts,
    target_opts: InstallTargetOpts,
) -> Result<Arc<State>> {
    // We require --pid=host
    let pid = std::fs::read_link("/proc/1/exe").context("reading /proc/1/exe")?;
    let pid = pid
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Non-UTF8 /proc/1/exe"))?;
    if !pid.contains("systemd") {
        anyhow::bail!("This command must be run with --pid=host")
    }

    // This command currently *must* be run inside a privileged container.
    let container_info = crate::containerenv::get_container_execution_info()?;
    if !container_info.engine.starts_with("podman") {
        anyhow::bail!("Currently this command only supports being executed via podman");
    }
    if container_info.imageid.is_empty() {
        anyhow::bail!("Invalid empty imageid");
    }
    let source_imageref = ostree_container::ImageReference {
        transport: ostree_container::Transport::ContainerStorage,
        name: container_info.image.clone(),
    };
    // Find the exact digested image we are running
    let source_digest = crate::podman::imageid_to_digest(&container_info.imageid)?;

    // Even though we require running in a container, the mounts we create should be specific
    // to this process, so let's enter a private mountns to avoid leaking them.
    if std::env::var_os("BOOTC_SKIP_UNSHARE").is_none() {
        super::cli::ensure_self_unshared_mount_namespace().await?;
    }

    // Let's ensure we have a tmpfs on /tmp, because we need that to write the SELinux label
    // (it won't work on the default overlayfs)
    if nix::sys::statfs::statfs("/tmp")?.filesystem_type() != nix::sys::statfs::TMPFS_MAGIC {
        Task::new("Creating tmpfs on /tmp", "mount")
            .quiet()
            .args(["-t", "tmpfs", "tmpfs", "/tmp"])
            .run()?;
    }

    // Now, deal with SELinux state.
    let srcdata = gather_source_data()?;
    let override_disable_selinux =
        reexecute_self_for_selinux_if_needed(&srcdata, config_opts.disable_selinux)?;

    if std::env::var_os("BOOTC_RHEL8_HACK").is_none() {
        // Create our global (read-only) state which gets wrapped in an Arc
        // so we can pass it to worker threads too. Right now this just
        // combines our command line options along with some bind mounts from the host.
        // Overmount /var/tmp with the host's, so we can use it to share state
        bind_mount_from_host("/var/tmp", "/var/tmp")?;
    }
    let state = Arc::new(State {
        override_disable_selinux,
        source_imageref,
        source_digest,
        config_opts,
        target_opts,
    });

    Ok(state)
}

async fn install_to_filesystem_impl(state: &State, rootfs: &mut RootSetup) -> Result<()> {
    if state.override_disable_selinux {
        rootfs.kargs.push("selinux=0".to_string());
    }
    // This is interpreted by our GRUB fragment
    if state.config_opts.ignition_file.is_some() {
        rootfs
            .kargs
            .push(crate::ignition::PLATFORM_METAL_KARG.to_string());
        rootfs
            .kargs
            .push(crate::bootloader::IGNITION_VARIABLE.to_string());
    }

    // Write the aleph data that captures the system state at the time of provisioning for aid in future debugging.
    {
        let aleph = initialize_ostree_root_from_self(state, rootfs).await?;
        rootfs
            .rootfs_fd
            .atomic_replace_with(BOOTC_ALEPH_PATH, |f| {
                serde_json::to_writer(f, &aleph)?;
                anyhow::Ok(())
            })
            .context("Writing aleph version")?;
    }

    let boot_uuid = rootfs.get_boot_uuid()?;
    crate::bootloader::install_via_bootupd(&rootfs.device, &rootfs.rootfs, boot_uuid)?;
    tracing::debug!("Installed bootloader");

    // If Ignition is specified, enable it
    if let Some(ignition_file) = state.config_opts.ignition_file.as_deref() {
        let src = std::fs::File::open(ignition_file)
            .with_context(|| format!("Opening {ignition_file}"))?;
        let bootfs = rootfs.rootfs.join("boot");
        crate::ignition::write_ignition(&bootfs, &state.config_opts.ignition_hash, &src)?;
        crate::ignition::enable_firstboot(&bootfs)?;
        println!("Installed Ignition config from {ignition_file}");
    }

    // ostree likes to have the immutable bit on the physical sysroot to ensure
    // that it doesn't accumulate junk; all system state should be in deployments.
    Task::new("Setting root immutable bit", "chattr")
        .cwd(&rootfs.rootfs_fd)?
        .args(["+i", "."])
        .run()?;

    // Finalize mounted filesystems
    let bootfs = rootfs.rootfs.join("boot");
    for fs in [bootfs.as_path(), rootfs.rootfs.as_path()] {
        finalize_filesystem(fs)?;
    }

    Ok(())
}

fn installation_complete() {
    println!("Installation complete!");
}

/// Implementation of the `bootc install` CLI command.
pub(crate) async fn install(opts: InstallOpts) -> Result<()> {
    let block_opts = opts.block_opts;
    let state = prepare_install(opts.config_opts, opts.target_opts).await?;

    // This is all blocking stuff
    let mut rootfs = {
        tokio::task::spawn_blocking(move || baseline::install_create_rootfs(block_opts)).await??
    };

    install_to_filesystem_impl(&state, &mut rootfs).await?;

    // Drop all data about the root except the path to ensure any file descriptors etc. are closed.
    let rootfs_path = rootfs.rootfs.clone();
    drop(rootfs);

    Task::new_and_run(
        "Unmounting filesystems",
        "umount",
        ["-R", rootfs_path.as_str()],
    )?;

    installation_complete();

    Ok(())
}

#[context("Verifying empty rootfs")]
fn require_empty_rootdir(rootfs_fd: &Dir) -> Result<()> {
    for e in rootfs_fd.entries()? {
        let e = e?;
        let name = e.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| anyhow!("Invalid non-UTF8 filename: {name:?}"))?;
        if name == LOST_AND_FOUND {
            continue;
        }
        // There must be a boot directory (that is empty)
        if name == BOOT {
            let mut entries = rootfs_fd.read_dir(BOOT)?;
            if let Some(e) = entries.next() {
                let e = e?;
                let name = e.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| anyhow!("Invalid non-UTF8 filename: {name:?}"))?;
                if matches!(name, LOST_AND_FOUND | crate::bootloader::EFI_DIR) {
                    continue;
                }
                anyhow::bail!("Non-empty boot directory, found {name:?}");
            }
        } else {
            anyhow::bail!("Non-empty root filesystem; found {name:?}");
        }
    }
    Ok(())
}

/// Implementation of the `bootc install-to-filsystem` CLI command.
pub(crate) async fn install_to_filesystem(opts: InstallToFilesystemOpts) -> Result<()> {
    // Gather global state, destructuring the provided options
    let state = prepare_install(opts.config_opts, opts.target_opts).await?;
    let fsopts = opts.filesystem_opts;

    let root_path = &fsopts.root_path;
    let rootfs_fd = Dir::open_ambient_dir(root_path, cap_std::ambient_authority())
        .with_context(|| format!("Opening target root directory {root_path}"))?;
    if fsopts.wipe {
        let rootfs_fd = rootfs_fd.try_clone()?;
        println!("Wiping contents of root");
        tokio::task::spawn_blocking(move || {
            for e in rootfs_fd.entries()? {
                let e = e?;
                rootfs_fd.remove_all_optional(e.file_name())?;
            }
            anyhow::Ok(())
        })
        .await??;
    } else {
        require_empty_rootdir(&rootfs_fd)?;
    }

    // Gather data about the root filesystem
    let inspect = crate::mount::inspect_filesystem(&fsopts.root_path)?;

    // We support overriding the mount specification for root (i.e. LABEL vs UUID versus
    // raw paths).
    let root_mount_spec = if let Some(s) = fsopts.root_mount_spec {
        s
    } else {
        let mut uuid = inspect
            .uuid
            .ok_or_else(|| anyhow!("No filesystem uuid found in target root"))?;
        uuid.insert_str(0, "UUID=");
        tracing::debug!("root {uuid}");
        uuid
    };
    tracing::debug!("Root mount spec: {root_mount_spec}");

    // Verify /boot is a separate mount
    {
        let root_dev = rootfs_fd.dir_metadata()?.dev();
        let boot_dev = rootfs_fd
            .symlink_metadata_optional(BOOT)?
            .ok_or_else(|| {
                anyhow!("No /{BOOT} directory found in root; this is is currently required")
            })?
            .dev();
        tracing::debug!("root_dev={root_dev} boot_dev={boot_dev}");
        if root_dev == boot_dev {
            anyhow::bail!("/{BOOT} must currently be a separate mounted filesystem");
        }
    }
    // Find the UUID of /boot because we need it for GRUB.
    let boot_path = fsopts.root_path.join(BOOT);
    let boot_uuid = crate::mount::inspect_filesystem(&boot_path)
        .context("Inspecting /{BOOT}")?
        .uuid
        .ok_or_else(|| anyhow!("No UUID found for /{BOOT}"))?;
    tracing::debug!("boot UUID: {boot_uuid}");

    // Find the real underlying backing device for the root.  This is currently just required
    // for GRUB (BIOS) and in the future zipl (I think).
    let backing_device = {
        let mut dev = inspect.source;
        loop {
            tracing::debug!("Finding parents for {dev}");
            let mut parents = crate::blockdev::find_parent_devices(&dev)?.into_iter();
            let parent = if let Some(f) = parents.next() {
                f
            } else {
                break;
            };
            if let Some(next) = parents.next() {
                anyhow::bail!(
                    "Found multiple parent devices {parent} and {next}; not currently supported"
                );
            }
            dev = parent;
        }
        dev
    };
    tracing::debug!("Backing device: {backing_device}");

    let rootarg = format!("root={root_mount_spec}");
    let boot = if let Some(spec) = fsopts.boot_mount_spec {
        MountSpec::new(&spec, "/boot")
    } else {
        MountSpec::new_uuid_src(&boot_uuid, "/boot")
    };
    // By default, we inject a boot= karg because things like FIPS compliance currently
    // require checking in the initramfs.
    let bootarg = format!("boot={}", &boot.source);
    let kargs = vec![rootarg, RW_KARG.to_string(), bootarg];

    let mut rootfs = RootSetup {
        device: backing_device.into(),
        rootfs: fsopts.root_path,
        rootfs_fd,
        boot,
        kargs,
    };

    install_to_filesystem_impl(&state, &mut rootfs).await?;

    // Drop all data about the root except the path to ensure any file descriptors etc. are closed.
    drop(rootfs);

    installation_complete();

    Ok(())
}

#[test]
fn install_opts_serializable() {
    let c: InstallOpts = serde_json::from_value(serde_json::json!({
        "device": "/dev/vda"
    }))
    .unwrap();
    assert_eq!(c.block_opts.device, "/dev/vda");
}
