//! DRM device lookup.

use std::path::PathBuf;

/// The render node (`/dev/dri/renderD*`) of the GPU behind `device`, which
/// may be either its primary or its render node. The compositor advertises
/// its GPU this way, and decoding on that same GPU keeps frames importable
/// without a cross-device copy.
pub fn render_node(device: libc::dev_t) -> Option<PathBuf> {
    std::fs::read_dir(sysfs_device(device).join("drm"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .find(|name| name.to_string_lossy().starts_with("renderD"))
        .map(|name| PathBuf::from("/dev/dri").join(name))
}

/// The kernel driver bound to the GPU behind `device`, e.g. `amdgpu`.
pub fn kernel_driver(device: libc::dev_t) -> Option<String> {
    let link = std::fs::read_link(sysfs_device(device).join("driver")).ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

fn sysfs_device(device: libc::dev_t) -> PathBuf {
    PathBuf::from(format!(
        "/sys/dev/char/{}:{}/device",
        libc::major(device),
        libc::minor(device)
    ))
}
