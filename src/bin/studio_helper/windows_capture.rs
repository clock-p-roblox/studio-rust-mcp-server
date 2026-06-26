#[cfg(target_os = "windows")]
fn decode_ipv4_addr(value: u32) -> Ipv4Addr {
    Ipv4Addr::from(u32::from_be(value))
}

#[cfg(target_os = "windows")]
fn decode_ipv4_port(value: u32) -> u16 {
    u16::from_be(value as u16)
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn enum_top_windows_callback(hwnd: HWND, lparam: LPARAM) -> i32 {
    let search = &mut *(lparam as *mut TopWindowSearch);
    if IsWindowVisible(hwnd) == 0 {
        return 1;
    }

    let mut window_pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut window_pid);

    let mut rect = RECT::default();
    let used_window_rect = if GetWindowRect(hwnd, &mut rect) != 0 {
        true
    } else {
        GetClientRect(hwnd, &mut rect) != 0
    };
    if !used_window_rect {
        rect = RECT::default();
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;

    let title_length = GetWindowTextLengthW(hwnd);
    let mut title_buffer = vec![0u16; title_length as usize + 1];
    let copied = GetWindowTextW(hwnd, title_buffer.as_mut_ptr(), title_buffer.len() as i32);
    let title = String::from_utf16_lossy(&title_buffer[..copied.max(0) as usize]);

    search.candidates.push(WindowCandidate {
        hwnd,
        process_id: window_pid,
        title,
        width: width.max(0),
        height: height.max(0),
    });
    1
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn enum_child_windows_callback(hwnd: HWND, lparam: LPARAM) -> i32 {
    let search = &mut *(lparam as *mut ChildWindowSearch);
    if IsWindowVisible(hwnd) == 0 {
        return 1;
    }

    let mut rect = RECT::default();
    if GetClientRect(hwnd, &mut rect) == 0 {
        return 1;
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width < search.min_width || height < search.min_height {
        return 1;
    }

    search.candidates.push(ChildWindowCandidate {
        hwnd,
        width,
        height,
    });
    1
}

#[cfg(target_os = "windows")]
fn resolve_peer_process_id(peer_addr: SocketAddr, helper_port: u16) -> Result<Option<u32>> {
    let SocketAddr::V4(peer_v4) = peer_addr else {
        return Ok(None);
    };

    let mut buffer_size = 0u32;
    unsafe {
        GetExtendedTcpTable(
            null_mut(),
            &mut buffer_size,
            0,
            AF_INET as u32,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        );
    }
    if buffer_size == 0 {
        return Err(eyre!("GetExtendedTcpTable did not report a buffer size"));
    }

    let mut buffer = vec![0u8; buffer_size as usize + size_of::<MIB_TCPROW_OWNER_PID>()];
    let result = unsafe {
        GetExtendedTcpTable(
            buffer.as_mut_ptr().cast(),
            &mut buffer_size,
            0,
            AF_INET as u32,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        )
    };
    if result != 0 {
        return Err(eyre!("GetExtendedTcpTable failed with code {result}"));
    }

    let table = unsafe { &*(buffer.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>()) };
    let rows =
        unsafe { std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize) };
    let peer_ip = *peer_v4.ip();
    let peer_port = peer_v4.port();
    let helper_ip = Ipv4Addr::LOCALHOST;
    for row in rows {
        let local_ip = decode_ipv4_addr(row.dwLocalAddr);
        let remote_ip = decode_ipv4_addr(row.dwRemoteAddr);
        let local_port = decode_ipv4_port(row.dwLocalPort);
        let remote_port = decode_ipv4_port(row.dwRemotePort);
        if local_ip == peer_ip
            && local_port == peer_port
            && remote_ip == helper_ip
            && remote_port == helper_port
        {
            return Ok(Some(row.dwOwningPid));
        }
    }

    Ok(None)
}

#[cfg(not(target_os = "windows"))]
fn resolve_peer_process_id(_peer_addr: SocketAddr, _helper_port: u16) -> Result<Option<u32>> {
    Ok(None)
}

async fn resolve_peer_process_id_with_retry(
    peer_addr: SocketAddr,
    helper_port: u16,
) -> Result<Option<u32>> {
    for attempt in 0..10 {
        let peer_addr_for_attempt = peer_addr;
        let pid = tokio::task::spawn_blocking(move || {
            resolve_peer_process_id(peer_addr_for_attempt, helper_port)
        })
        .await
        .map_err(|error| eyre!("peer pid lookup task failed: {error}"))??;
        if let Some(pid) = pid {
            return Ok(Some(pid));
        }
        if attempt < 9 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    Ok(None)
}

#[cfg(target_os = "windows")]
fn collect_visible_child_windows(
    parent_hwnd: HWND,
    min_width: i32,
    min_height: i32,
) -> Vec<ChildWindowCandidate> {
    let mut search = ChildWindowSearch {
        min_width,
        min_height,
        candidates: Vec::new(),
    };
    unsafe {
        EnumChildWindows(
            parent_hwnd,
            Some(enum_child_windows_callback),
            (&mut search as *mut ChildWindowSearch) as LPARAM,
        );
    }
    search.candidates
}

#[cfg(target_os = "windows")]
fn find_studio_capture_target_for_pid(studio_pid: u32) -> Result<(HWND, HWND, String)> {
    let mut search = TopWindowSearch {
        candidates: Vec::new(),
    };
    unsafe {
        EnumWindows(
            Some(enum_top_windows_callback),
            (&mut search as *mut TopWindowSearch) as LPARAM,
        );
    }
    let all_candidates = search.candidates;
    let exact_candidates: Vec<_> = all_candidates
        .iter()
        .filter(|candidate| candidate.process_id == studio_pid)
        .cloned()
        .collect();
    let studio_window = exact_candidates
        .into_iter()
        .max_by_key(|candidate| {
            let title_score = if candidate.title.contains("Roblox Studio") { 1 } else { 0 };
            (title_score, i64::from(candidate.width) * i64::from(candidate.height))
        })
        .or_else(|| {
            let studio_named: Vec<_> = all_candidates
                .iter()
                .filter(|candidate| candidate.title.contains("Roblox Studio"))
                .cloned()
                .collect();
            if studio_named.len() == 1 {
                let candidate = studio_named.into_iter().next().unwrap();
                tracing::warn!(
                    requested_pid = studio_pid,
                    fallback_pid = candidate.process_id,
                    title = candidate.title,
                    "did not find an exact Studio window pid match; falling back to the only visible Roblox Studio window"
                );
                Some(candidate)
            } else {
                None
            }
        })
        .ok_or_else(|| {
            let visible_studio_windows: Vec<_> = all_candidates
                .iter()
                .filter(|candidate| candidate.title.contains("Roblox Studio"))
                .map(|candidate| format!("pid={} title={}", candidate.process_id, candidate.title))
                .collect();
            if visible_studio_windows.is_empty() {
                eyre!("could not find any visible Roblox Studio window while resolving pid {studio_pid}")
            } else {
                eyre!(
                    "could not find an exact visible Roblox Studio window for pid {studio_pid}; visible Studio windows: {}",
                    visible_studio_windows.join(" | ")
                )
            }
        })?;

    let descendants = collect_visible_child_windows(studio_window.hwnd, 500, 400);
    let content_area = descendants
        .iter()
        .find(|candidate| {
            candidate.height > 600 && candidate.height < 750 && candidate.width > 2000
        })
        .map(|candidate| candidate.hwnd)
        .or_else(|| {
            descendants
                .iter()
                .filter(|candidate| candidate.width > candidate.height)
                .max_by_key(|candidate| i64::from(candidate.width) * i64::from(candidate.height))
                .map(|candidate| candidate.hwnd)
        })
        .unwrap_or(studio_window.hwnd);

    let viewport_candidates = collect_visible_child_windows(content_area, 500, 400);
    let viewport = viewport_candidates
        .iter()
        .find(|candidate| {
            candidate.width < 1660
                && candidate.height < 680
                && candidate.width > 1000
                && candidate.height > 500
        })
        .map(|candidate| candidate.hwnd)
        .or_else(|| {
            viewport_candidates
                .iter()
                .find(|candidate| candidate.width > 1000 && candidate.height > 500)
                .map(|candidate| candidate.hwnd)
        })
        .unwrap_or(studio_window.hwnd);

    Ok((studio_window.hwnd, viewport, studio_window.title))
}
