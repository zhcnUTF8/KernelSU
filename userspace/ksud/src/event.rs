use anyhow::{bail, Context, Result};
use log::{info, warn};
use std::{collections::HashMap, path::Path};

use crate::{
    assets, defs, mount,
    utils::{self, ensure_clean_dir, ensure_dir_exists},
};

fn mount_partition(partition: &str, lowerdir: &mut Vec<String>) -> Result<()> {
    if lowerdir.is_empty() {
        warn!("partition: {partition} lowerdir is empty");
        return Ok(());
    }

    // if /partition is a symlink and linked to /system/partition, then we don't need to overlay it separately
    if Path::new(&format!("/{partition}")).read_link().is_ok() {
        warn!("partition: {partition} is a symlink");
        return Ok(());
    }

    // handle stock mounts under /partition, we should restore the mount point after overlay
    // because the overlayfs mount will "overlay" the bind mount such as /vendor/bt_firmware, /vendor/dsp
    // which will cause the system bootloop or bluetooth/dsp not working
    let stock_mount = mount::StockMount::new(&format!("/{partition}/"))
        .with_context(|| format!("get stock mount of partition: {partition} failed"))?;

    // add /partition as the lowerest dir
    let lowest_dir = format!("/{partition}");
    lowerdir.push(lowest_dir.clone());

    let lowerdir = lowerdir.join(":");
    info!("partition: {partition} lowerdir: {lowerdir}");

    let result = mount::mount_overlay(&lowerdir, &lowest_dir);

    if let Err(e) = stock_mount.remount() {
        if result.is_ok() {
            // if mount overlay ok but stock remount failed, we should umount overlay
            warn!("remount stock failed: {:?}, umount overlay {lowest_dir}", e);
            if mount::umount_dir(&lowest_dir).is_err() {
                warn!("umount overlay {lowest_dir} failed");
            }
        }
    }

    result
}

pub fn mount_systemlessly(module_dir: &str) -> Result<()> {
    // construct overlay mount params
    let dir = std::fs::read_dir(module_dir);
    let Ok(dir) = dir else {
            bail!("open {} failed", defs::MODULE_DIR);
        };

    let mut system_lowerdir: Vec<String> = Vec::new();

    let partition = vec!["vendor", "product", "system_ext", "odm", "oem"];
    let mut partition_lowerdir: HashMap<String, Vec<String>> = HashMap::new();
    for ele in &partition {
        partition_lowerdir.insert((*ele).to_string(), Vec::new());
    }

    for entry in dir.flatten() {
        let module = entry.path();
        if !module.is_dir() {
            continue;
        }
        let disabled = module.join(defs::DISABLE_FILE_NAME).exists();
        if disabled {
            info!("module: {} is disabled, ignore!", module.display());
            continue;
        }

        let module_system = Path::new(&module).join("system");
        if module_system.exists() {
            system_lowerdir.push(format!("{}", module_system.display()));
        }

        for part in &partition {
            // if /partition is a mountpoint, we would move it to $MODPATH/$partition when install
            // otherwise it must be a symlink and we don't need to overlay!
            let part_path = Path::new(&module).join(part);
            if !part_path.exists() {
                continue;
            }
            if let Some(v) = partition_lowerdir.get_mut(*part) {
                v.push(format!("{}", part_path.display()));
            }
        }
    }

    // mount /system first
    if let Err(e) = mount_partition("system", &mut system_lowerdir) {
        warn!("mount system failed: {e}");
    }

    // mount other partitions
    for (k, mut v) in partition_lowerdir {
        if let Err(e) = mount_partition(&k, &mut v) {
            warn!("mount {k} failed: {e}");
        }
    }

    Ok(())
}

pub fn on_post_data_fs() -> Result<()> {
    crate::ksu::report_post_fs_data();

    if utils::has_magisk() {
        warn!("Magisk detected, skip post-fs-data!");
        return Ok(());
    }

    utils::umask(0);

    let module_update_img = defs::MODULE_UPDATE_IMG;
    let module_img = defs::MODULE_IMG;
    let module_dir = defs::MODULE_DIR;
    let module_update_flag = Path::new(defs::WORKING_DIR).join(defs::UPDATE_FILE_NAME);

    // modules.img is the default image
    let mut target_update_img = &module_img;

    // we should clean the module mount point if it exists
    ensure_clean_dir(module_dir)?;

    assets::ensure_binaries().with_context(|| "Failed to extract bin assets")?;

    if Path::new(module_update_img).exists() {
        if module_update_flag.exists() {
            // if modules_update.img exists, and the the flag indicate this is an update
            // this make sure that if the update failed, we will fallback to the old image
            // if we boot succeed, we will rename the modules_update.img to modules.img #on_boot_complete
            target_update_img = &module_update_img;
            // And we should delete the flag immediately
            std::fs::remove_file(module_update_flag)?;
        } else {
            // if modules_update.img exists, but the flag not exist, we should delete it
            std::fs::remove_file(module_update_img)?;
        }
    }

    // If there isn't any image exist, do nothing for module!
    if !Path::new(target_update_img).exists() {
        return Ok(());
    }

    // we should always mount the module.img to module dir
    // becuase we may need to operate the module dir in safe mode
    info!("mount module image: {target_update_img} to {module_dir}");
    mount::AutoMountExt4::try_new(target_update_img, module_dir, false)
        .with_context(|| "mount module image failed".to_string())?;

    // check safe mode first.
    if crate::utils::is_safe_mode() {
        warn!("safe mode, skip post-fs-data scripts and disable all modules!");
        if let Err(e) = crate::module::disable_all_modules() {
            warn!("disable all modules failed: {}", e);
        }
        return Ok(());
    }

    // Then exec common post-fs-data scripts
    if let Err(e) = crate::module::exec_common_scripts("post-fs-data.d", true) {
        warn!("exec common post-fs-data scripts failed: {}", e);
    }

    // load sepolicy.rule
    if crate::module::load_sepolicy_rule().is_err() {
        warn!("load sepolicy.rule failed");
    }

    // exec modules post-fs-data scripts
    // TODO: Add timeout
    if let Err(e) = crate::module::exec_post_fs_data() {
        warn!("exec post-fs-data scripts failed: {}", e);
    }

    // load system.prop
    if let Err(e) = crate::module::load_system_prop() {
        warn!("load system.prop failed: {}", e);
    }

    // Finally, we should do systemless mount
    // But we should umount all stock overlayfs and remount them after module mounted
    let stock_overlay = mount::StockOverlay::new();
    stock_overlay.umount_all();

    // mount moduke systemlessly by overlay
    if let Err(e) = mount_systemlessly(module_dir) {
        warn!("do systemless mount failed: {}", e);
    }

    stock_overlay.mount_all();

    Ok(())
}

pub fn on_services() -> Result<()> {
    utils::umask(0);

    if utils::has_magisk() {
        warn!("Magisk detected, skip services!");
        return Ok(());
    }

    if crate::utils::is_safe_mode() {
        warn!("safe mode, skip module service scripts");
        return Ok(());
    }

    if let Err(e) = crate::module::exec_common_scripts("service.d", false) {
        warn!("Failed to exec common service scripts: {}", e);
    }
    if let Err(e) = crate::module::exec_services() {
        warn!("Failed to exec service scripts: {}", e);
    }

    Ok(())
}

pub fn on_boot_completed() -> Result<()> {
    crate::ksu::report_boot_complete();
    info!("on_boot_completed triggered!");
    let module_update_img = Path::new(defs::MODULE_UPDATE_IMG);
    let module_img = Path::new(defs::MODULE_IMG);
    if module_update_img.exists() {
        // this is a update and we successfully booted
        if std::fs::rename(module_update_img, module_img).is_err() {
            warn!("Failed to rename images, copy it now.",);
            std::fs::copy(module_update_img, module_img)
                .with_context(|| "Failed to copy images")?;
            std::fs::remove_file(module_update_img).with_context(|| "Failed to remove image!")?;
        }
    }
    Ok(())
}

pub fn daemon() -> Result<()> {
    Ok(())
}

pub fn install() -> Result<()> {
    ensure_dir_exists(defs::ADB_DIR)?;
    std::fs::copy("/proc/self/exe", defs::DAEMON_PATH)?;

    // install binary assets
    assets::ensure_binaries().with_context(|| "Failed to extract assets")?;

    #[cfg(target_os = "android")]
    link_ksud_to_bin()?;

    Ok(())
}

#[cfg(target_os = "android")]
fn link_ksud_to_bin() -> Result<()> {
    use std::path::PathBuf;
    let ksu_bin = PathBuf::from(defs::DAEMON_PATH);
    let ksu_bin_link = PathBuf::from(defs::DAEMON_LINK_PATH);
    if ksu_bin.exists() && !ksu_bin_link.exists() {
        std::os::unix::fs::symlink(&ksu_bin, &ksu_bin_link)?;
    }
    Ok(())
}
