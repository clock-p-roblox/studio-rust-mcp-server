#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
async fn resolve_capture_pid(
    app: &AppState,
    place_id: Option<&str>,
    task_id: Option<&str>,
) -> Result<(Option<String>, u32)> {
    let state = app.state.lock().await;
    let selected = if let Some(place_id) = place_id {
        let instance_id = select_instance_for_route(place_id, task_id, &state.instances)
            .ok_or_else(|| eyre!("no active Studio plugin registered for placeId {place_id}"))?;
        let instance = state
            .instances
            .get(&instance_id)
            .ok_or_else(|| eyre!("selected helper instance disappeared"))?;
        (Some(place_id.to_owned()), instance.studio_pid)
    } else if state.instances.len() == 1 {
        let instance = state
            .instances
            .values()
            .next()
            .ok_or_else(|| eyre!("no active Studio plugin registered"))?;
        (Some(instance.place_id.clone()), instance.studio_pid)
    } else if state.instances.is_empty() {
        return Err(eyre!("no active Studio plugin registered"));
    } else {
        return Err(eyre!(
            "multiple Studio instances are active; pass placeId explicitly"
        ));
    };

    let studio_pid = selected.1.ok_or_else(|| {
        eyre!("helper could not resolve a Studio process id for the selected plugin instance")
    })?;
    Ok((selected.0, studio_pid))
}

async fn capture_screenshot_png(
    app: AppState,
    place_id: Option<&str>,
    task_id: Option<&str>,
) -> Result<CapturedScreenshot> {
    #[cfg(target_os = "windows")]
    {
        let (resolved_place_id, studio_pid) = resolve_capture_pid(&app, place_id, task_id).await?;
        let (studio_hwnd, viewport_hwnd, window_title) =
            find_studio_capture_target_for_pid(studio_pid)?;
        let script_template = r#"
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Win32 {
    [DllImport("user32.dll")]
    public static extern bool GetClientRect(IntPtr hWnd, out RECT rect);

    [DllImport("user32.dll")]
    public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdcBlt, int nFlags);

    [DllImport("user32.dll")]
    public static extern bool IsIconic(IntPtr hWnd);

    [DllImport("user32.dll")]
    public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);

    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr hWnd);

    public struct RECT {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }
}
"@

$studioHwnd = [IntPtr]::new(__STUDIO_HWND__)
$viewportHwnd = [IntPtr]::new(__VIEWPORT_HWND__)

$rect = New-Object Win32+RECT
[Win32]::GetClientRect($viewportHwnd, [ref]$rect) | Out-Null
$width = $rect.Right - $rect.Left
$height = $rect.Bottom - $rect.Top
if ($width -le 0 -or $height -le 0) {
    throw 'Selected Studio viewport has invalid bounds'
}

$capture = {
    param($windowHandle, $w, $h)

    $bitmap = New-Object System.Drawing.Bitmap $w, $h
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $hdc = $graphics.GetHdc()
    $ok = [Win32]::PrintWindow($windowHandle, $hdc, 2)
    $graphics.ReleaseHdc($hdc)

    return @{
        ok = $ok
        bitmap = $bitmap
        graphics = $graphics
    }
}

$result = & $capture $viewportHwnd $width $height
$sampleX = [Math]::Min(10, [Math]::Max($width - 1, 0))
$sampleY = [Math]::Min(10, [Math]::Max($height - 1, 0))
if (-not $result.ok -or $result.bitmap.GetPixel($sampleX, $sampleY).ToArgb() -eq [System.Drawing.Color]::Black.ToArgb()) {
    if ([Win32]::IsIconic($studioHwnd)) {
        [Win32]::ShowWindow($studioHwnd, 9) | Out-Null
    }
    [Win32]::SetForegroundWindow($studioHwnd) | Out-Null
    Start-Sleep -Milliseconds 300
    $result.graphics.Dispose()
    $result.bitmap.Dispose()
    $result = & $capture $viewportHwnd $width $height
}

if (-not $result.ok) {
    throw 'PrintWindow failed for the selected Studio viewport'
}

$bitmap = New-Object System.Drawing.Bitmap $width, $height
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.DrawImage($result.bitmap, 0, 0)
$memory = New-Object System.IO.MemoryStream
$bitmap.Save($memory, [System.Drawing.Imaging.ImageFormat]::Png)
$bytes = $memory.ToArray()
[Console]::OpenStandardOutput().Write($bytes, 0, $bytes.Length)
$result.graphics.Dispose()
$result.bitmap.Dispose()
$graphics.Dispose()
$bitmap.Dispose()
$memory.Dispose()
"#;
        let script = script_template
            .replace("__STUDIO_HWND__", &(studio_hwnd as usize).to_string())
            .replace("__VIEWPORT_HWND__", &(viewport_hwnd as usize).to_string());
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .wrap_err("failed to launch powershell screenshot helper")?;
        if !output.status.success() {
            return Err(eyre!(
                "screenshot command failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        tracing::info!(
            studio_pid,
            window_title,
            byte_len = output.stdout.len(),
            "captured Studio screenshot bytes from helper"
        );
        Ok(CapturedScreenshot {
            resolved_place_id,
            png_bytes: output.stdout,
            window_title,
        })
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
        let _ = place_id;
        let _ = task_id;
        Err(eyre!(
            "take_screenshot is only available on Windows helper builds"
        ))
    }
}

async fn take_screenshot(
    app: AppState,
    place_id: Option<&str>,
    task_id: Option<&str>,
) -> Result<String> {
    #[cfg(target_os = "windows")]
    {
        let captured = capture_screenshot_png(app, place_id, task_id).await?;
        let output_dir = helper_data_dir()?.join("screenshots");
        fs::create_dir_all(&output_dir)?;
        let file_name = if let Some(place_id) = captured.resolved_place_id.as_deref() {
            format!("studio-{place_id}-{}.png", now_stamp())
        } else {
            format!("studio-{}.png", now_stamp())
        };
        let output_path = output_dir.join(file_name);
        fs::write(&output_path, captured.png_bytes)?;
        tracing::info!(path = %output_path.display(), window_title = captured.window_title, "saved Studio screenshot debug file from helper");
        Ok(output_path.display().to_string())
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
        let _ = place_id;
        let _ = task_id;
        Err(eyre!(
            "take_screenshot is only available on Windows helper builds"
        ))
    }
}

async fn upload_runtime_screenshot(
    app: AppState,
    payload: RuntimeScreenshotRequest,
) -> Result<RuntimeScreenshotResponse> {
    let place_id = match payload.place_id.as_deref() {
        Some(value) => Some(sanitize_place_id(value)?),
        None => None,
    };
    let session_id = sanitize_identifier("session_id", &payload.session_id)?;
    let runtime_id = sanitize_identifier("runtime_id", &payload.runtime_id)?;
    let task_id = maybe_sanitize_identifier("task_id", payload.task_id.as_deref())?;
    let captured =
        capture_screenshot_png(app.clone(), place_id.as_deref(), task_id.as_deref()).await?;
    let final_place_id = captured
        .resolved_place_id
        .clone()
        .or(place_id)
        .ok_or_else(|| eyre!("helper could not resolve placeId for runtime screenshot upload"))?;
    let upload_url = {
        let state = app.state.lock().await;
        validate_runtime_screenshot_upload_url(
            &payload.upload_url,
            &final_place_id,
            task_id.as_deref(),
            &state,
            &app.helper,
        )?
    };
    let sink_response = post_runtime_screenshot(
        &app.helper,
        &upload_url,
        &session_id,
        &runtime_id,
        &final_place_id,
        payload.tag.as_deref(),
        &captured.png_bytes,
    )
    .await?;
    tracing::info!(
        session_id,
        runtime_id,
        place_id = final_place_id,
        screenshot_path = sink_response.screenshot_path,
        window_title = captured.window_title,
        "uploaded runtime screenshot through helper"
    );
    Ok(RuntimeScreenshotResponse {
        session_id: sink_response.session_id,
        runtime_id: sink_response.runtime_id,
        place_id: final_place_id,
        screenshot_path: sink_response.screenshot_path,
        screenshot_rel_path: None,
        artifact_dir: sink_response.artifact_dir,
        session_metadata_path: sink_response.session_metadata_path,
        bytes_written: sink_response.bytes_written,
    })
}

fn read_studio_log(args: ReadStudioLogArgs) -> Result<ReadStudioLogResponse> {
    #[cfg(target_os = "windows")]
    {
        let local_app_data = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| eyre!("LOCALAPPDATA is not set"))?;
        let logs_dir = local_app_data.join("Roblox").join("logs");
        let (log_path, content) = read_latest_studio_log(&logs_dir)?;
        let mut all_lines: Vec<String> = content.lines().map(ToOwned::to_owned).collect();
        if let Some(pattern) = args.regex.as_ref() {
            let regex = Regex::new(pattern)?;
            all_lines.retain(|line| regex.is_match(line));
        }
        let total_lines = all_lines.len();
        let count = args.line_count.unwrap_or(total_lines.max(1)).max(1);
        let mut start_line = args.start_line.unwrap_or(1);
        if start_line < 0 {
            start_line = (total_lines as i64 + start_line + 1).max(1);
        }
        let start = start_line.clamp(1, total_lines.max(1) as i64) as usize;
        let end = if total_lines == 0 {
            0
        } else {
            (start + count - 1).min(total_lines)
        };
        let lines = if total_lines == 0 || end == 0 {
            Vec::new()
        } else {
            all_lines[(start - 1)..end].to_vec()
        };
        tracing::debug!(path = %log_path.display(), total_lines, returned_lines = lines.len(), "read Studio log from helper");
        Ok(ReadStudioLogResponse {
            path: log_path.display().to_string(),
            total_lines,
            range: [if total_lines == 0 { 0 } else { start }, end],
            lines,
        })
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = args;
        Err(eyre!(
            "read_studio_log is only available on Windows helper builds"
        ))
    }
}

#[cfg(target_os = "windows")]
fn list_candidate_log_files(logs_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut studio_files = Vec::new();
    let mut other_files = Vec::new();
    let mut entries: Vec<_> = fs::read_dir(logs_dir)
        .wrap_err_with(|| format!("failed to read {}", logs_dir.display()))?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("log") {
                let modified = entry.metadata().ok()?.modified().ok()?;
                Some((path, modified))
            } else {
                None
            }
        })
        .collect();
    entries.sort_by_key(|(_, modified)| *modified);
    entries.reverse();
    for (path, _) in entries {
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if file_name.contains("_Studio_") {
            studio_files.push(path);
        } else {
            other_files.push(path);
        }
    }
    if !studio_files.is_empty() {
        return Ok(studio_files);
    }
    if !other_files.is_empty() {
        tracing::warn!(
            path = %logs_dir.display(),
            "no explicit Studio log file matched '_Studio_', falling back to latest .log files"
        );
        return Ok(other_files);
    }
    Err(eyre!("no Studio log file found in {}", logs_dir.display()))
}

#[cfg(target_os = "windows")]
fn read_latest_studio_log(logs_dir: &Path) -> Result<(PathBuf, String)> {
    let candidates = list_candidate_log_files(logs_dir)?;
    let mut last_error = None;
    for path in candidates {
        match fs::read(&path) {
            Ok(bytes) => {
                let content = String::from_utf8_lossy(&bytes).into_owned();
                return Ok((path, content));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(path = %path.display(), "Studio log candidate disappeared before read; trying next file");
                last_error = Some(format!("{}: {}", path.display(), error));
            }
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                tracing::warn!(path = %path.display(), "Studio log candidate was not readable; trying next file");
                last_error = Some(format!("{}: {}", path.display(), error));
            }
            Err(error) => {
                last_error = Some(format!("{}: {}", path.display(), error));
            }
        }
    }
    Err(eyre!(
        "failed to read any Studio log file in {}{}",
        logs_dir.display(),
        last_error
            .as_deref()
            .map(|value| format!("; last error: {value}"))
            .unwrap_or_default()
    ))
}
