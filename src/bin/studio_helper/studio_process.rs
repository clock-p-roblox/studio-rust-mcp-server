use super::{ClaimedTask, HelperConfig, LaunchProcessRecord, ROBLOX_PLACE_UNIVERSE_LOOKUP_TIMEOUT};
use crate::text::{sanitize_place_id, summarize_error, trim};
use color_eyre::eyre::{eyre, Result, WrapErr};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Child;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "windows")]
use std::mem::size_of;
#[cfg(target_os = "windows")]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

#[cfg(target_os = "windows")]
use std::ptr::null_mut;
#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::HANDLE;
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

#[derive(Debug, Deserialize)]
struct RobloxPlaceUniverseResponse {
    #[serde(rename = "universeId")]
    universe_id: u64,
}

#[cfg(target_os = "windows")]
fn create_kill_on_close_job() -> Result<OwnedHandle> {
    let job = unsafe { CreateJobObjectW(null_mut(), null_mut()) };
    if job.is_null() {
        return Err(eyre!(
            "failed to create Studio kill-on-close job object: {}",
            std::io::Error::last_os_error()
        ));
    }

    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            let _ = OwnedHandle::from_raw_handle(job as _);
        }
        return Err(eyre!(
            "failed to configure Studio kill-on-close job object: {error}"
        ));
    }

    Ok(unsafe { OwnedHandle::from_raw_handle(job as _) })
}

#[cfg(target_os = "windows")]
fn assign_child_to_kill_on_close_job(job: &OwnedHandle, child: &Child) -> Result<()> {
    let process_handle = child.as_raw_handle() as HANDLE;
    let ok = unsafe { AssignProcessToJobObject(job.as_raw_handle() as HANDLE, process_handle) };
    if ok == 0 {
        return Err(eyre!(
            "failed to assign helper-launched Studio to kill-on-close job object: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

pub(super) fn now_stamp() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    millis.to_string()
}

#[cfg(target_os = "windows")]
fn resolve_studio_path(helper: &HelperConfig) -> Result<PathBuf> {
    if let Some(path) = helper.studio_path.as_ref() {
        if path.is_file() {
            return Ok(path.clone());
        }
        return Err(eyre!(
            "configured studio_path does not exist: {}",
            path.display()
        ));
    }
    if let Some(path) = env::var_os("CLOCK_P_STUDIO_PATH").map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        return Err(eyre!(
            "CLOCK_P_STUDIO_PATH does not exist: {}",
            path.display()
        ));
    }
    let versions_root = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|path| path.join("Roblox").join("Versions"))
        .ok_or_else(|| eyre!("cannot resolve LOCALAPPDATA for Studio discovery"))?;
    let entries = fs::read_dir(&versions_root).wrap_err_with(|| {
        format!(
            "cannot read Roblox Studio versions directory: {}",
            versions_root.display()
        )
    })?;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        let candidate = entry.path().join("RobloxStudioBeta.exe");
        if !candidate.is_file() {
            continue;
        }
        let modified = candidate
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH);
        candidates.push((modified, candidate));
    }
    candidates.sort_by_key(|(modified, _)| *modified);
    candidates.pop().map(|(_, path)| path).ok_or_else(|| {
        eyre!(
            "could not locate RobloxStudioBeta.exe under {}",
            versions_root.display()
        )
    })
}

#[cfg(target_os = "windows")]
pub(super) fn launch_studio_for_claim(
    helper: &HelperConfig,
    task: &ClaimedTask,
) -> Result<LaunchProcessRecord> {
    let studio_path = resolve_studio_path(helper)?;
    let mut command = std::process::Command::new(&studio_path);
    let launch_args = studio_launch_args_for_claim(task)?;
    for arg in &launch_args {
        command.arg(arg);
    }
    if let Some(parent) = studio_path.parent() {
        command.current_dir(parent);
    }
    let mut child = command
        .spawn()
        .wrap_err_with(|| format!("failed to launch Studio at {}", studio_path.display()))?;
    let kill_on_close_job = match create_kill_on_close_job() {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).wrap_err_with(|| {
                format!(
                    "failed to arm helper-launched Studio for helper-exit cleanup at {}",
                    studio_path.display()
                )
            });
        }
    };
    if let Err(error) = assign_child_to_kill_on_close_job(&kill_on_close_job, &child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error).wrap_err_with(|| {
            format!(
                "failed to tie helper-launched Studio to helper lifetime at {}",
                studio_path.display()
            )
        });
    }
    let studio_pid = child.id();
    tracing::info!(
        task_id = task.task_id.as_str(),
        place_id = task.place_id.as_str(),
        universe_id = task.universe_id.as_deref(),
        studio_pid,
        studio_path = %studio_path.display(),
        studio_args = ?launch_args,
        "launched Roblox Studio for claimed task"
    );
    Ok(LaunchProcessRecord {
        task_id: task.task_id.clone(),
        place_id: task.place_id.clone(),
        universe_id: task.universe_id.clone(),
        studio_pid,
        launched_by_helper: true,
        kill_on_close_job: Some(kill_on_close_job),
        child: Some(child),
    })
}

pub(super) fn studio_launch_args_for_claim(task: &ClaimedTask) -> Result<Vec<String>> {
    let universe_id = task.universe_id.as_deref().ok_or_else(|| {
        eyre!(
            "cannot launch Studio for placeId {} without universeId",
            task.place_id
        )
    })?;
    Ok(vec![
        "-task".to_owned(),
        "EditPlace".to_owned(),
        "-placeId".to_owned(),
        task.place_id.clone(),
        "-universeId".to_owned(),
        universe_id.to_owned(),
    ])
}

pub(super) async fn ensure_claimed_task_universe_id(
    helper: &HelperConfig,
    task: &mut ClaimedTask,
) -> Result<()> {
    if task
        .universe_id
        .as_deref()
        .is_some_and(|value| !trim(value).is_empty())
    {
        return Ok(());
    }
    let universe_id = resolve_universe_id_for_place(helper, &task.place_id).await?;
    tracing::info!(
        task_id = task.task_id.as_str(),
        place_id = task.place_id.as_str(),
        universe_id = universe_id.as_str(),
        "resolved universeId for Studio launch"
    );
    task.universe_id = Some(universe_id);
    Ok(())
}

async fn resolve_universe_id_for_place(helper: &HelperConfig, place_id: &str) -> Result<String> {
    let place_id = sanitize_place_id(place_id)?;
    let url = format!("https://apis.roblox.com/universes/v1/places/{place_id}/universe");
    let response = tokio::time::timeout(
        ROBLOX_PLACE_UNIVERSE_LOOKUP_TIMEOUT,
        helper.client.get(&url).send(),
    )
    .await
    .wrap_err_with(|| {
        format!(
            "timed out resolving universeId for placeId {place_id} after {}s",
            ROBLOX_PLACE_UNIVERSE_LOOKUP_TIMEOUT.as_secs()
        )
    })?
    .wrap_err_with(|| format!("failed to resolve universeId for placeId {place_id}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(eyre!(
            "failed to resolve universeId for placeId {place_id}: HTTP {status}: {}",
            summarize_error(&body)
        ));
    }
    let payload = response
        .json::<RobloxPlaceUniverseResponse>()
        .await
        .wrap_err_with(|| format!("failed to parse universeId response for placeId {place_id}"))?;
    Ok(payload.universe_id.to_string())
}
