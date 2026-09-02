use super::*;

/// Main window procedure
pub(super) unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTCLIENT as isize),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DISPLAYCHANGE | WM_DPICHANGED_MSG | WM_SETTINGCHANGE => {
            if msg == WM_DPICHANGED_MSG {
                let new_dpi = (wparam.0 & 0xFFFF) as u32;
                CURRENT_DPI.store(new_dpi, Ordering::Relaxed);
            }
            if msg == WM_SETTINGCHANGE {
                check_theme_change();
                check_language_change();
            }
            refresh_dpi();
            position_at_taskbar();
            render_layered();
            sync_tray_icon(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            let timer_id = wparam.0;
            match timer_id {
                TIMER_POLL => {
                    let auth_watch = {
                        let state = lock_state();
                        state.as_ref().map(|s| {
                            (
                                s.auth_error_paused_polling,
                                s.auth_watch_mode,
                                s.auth_watch_snapshot.clone(),
                            )
                        })
                    };
                    match auth_watch {
                        Some((true, watch_mode, previous_snapshot)) => {
                            let current_snapshot = poller::credential_watch_snapshot(watch_mode);
                            if current_snapshot != previous_snapshot {
                                let mut state = lock_state();
                                if let Some(s) = state.as_mut() {
                                    if s.auth_error_paused_polling
                                        && s.auth_watch_mode == watch_mode
                                    {
                                        s.auth_watch_snapshot = current_snapshot;
                                    }
                                }
                                drop(state);
                                request_poll(hwnd);
                            }
                        }
                        Some((false, _, _)) => {
                            request_scheduled_poll(hwnd);
                        }
                        None => {}
                    }
                }
                TIMER_COUNTDOWN => {
                    render_layered();
                    sync_tray_icon(hwnd);
                    schedule_countdown_timer();
                }
                TIMER_RESET_POLL => {
                    let should_poll = {
                        let state = lock_state();
                        state
                            .as_ref()
                            .map(|s| !s.auth_error_paused_polling)
                            .unwrap_or(false)
                    };
                    if should_poll {
                        request_scheduled_poll(hwnd);
                    }
                }
                TIMER_UPDATE_CHECK => {
                    begin_update_check(hwnd, false);
                }
                TIMER_WINDOW_STATE => {
                    sync_theme_window_visibility();
                }
                TIMER_MOUSE_CLICK => {
                    let _ = KillTimer(hwnd, TIMER_MOUSE_CLICK);
                    let pending = lock_state()
                        .as_mut()
                        .and_then(|state| state.pending_mouse_click.take());
                    if let Some(pending) = pending {
                        let _ = dispatch_mouse_event(
                            pending.surface_index,
                            &pending.object_id,
                            MouseEventKind::Click,
                        );
                    }
                }
                TIMER_TRAY_HOVER => {
                    clear_tray_mouse_hover_if_left(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_USAGE_UPDATED => {
            check_theme_change();
            check_language_change();
            render_layered();
            schedule_countdown_timer();
            sync_tray_icon(hwnd);
            LRESULT(0)
        }
        WM_APP_SETTINGS_UPDATED => {
            reload_external_settings(hwnd);
            LRESULT(0)
        }
        WM_APP_REFRESH_NOW => {
            request_poll(hwnd);
            LRESULT(0)
        }
        WM_APP_OPEN_DASHBOARD => {
            crate::dashboard::show(hwnd);
            LRESULT(0)
        }
        WM_APP_QUIT => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_APP_UPDATE_CHECK_COMPLETE => {
            schedule_auto_update_check(hwnd);
            LRESULT(0)
        }
        WM_APP_DRAG_REPOSITION => {
            if let Some(state) = lock_state().as_mut() {
                state.drag_reposition_pending = false;
            }
            position_at_taskbar();
            LRESULT(0)
        }
        WM_SETCURSOR if set_surface_cursor(hwnd) => LRESULT(1),
        WM_SETCURSOR => DefWindowProcW(hwnd, msg, wparam, lparam),
        WM_LBUTTONDOWN => {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let mut widget_rect = RECT::default();
            let _ = GetWindowRect(hwnd, &mut widget_rect);
            let widget_width = widget_rect.right - widget_rect.left;
            // The widget is embedded as a child of the taskbar it's docked on
            // (see position_custom_theme_internal), so its parent HWND is the
            // authoritative taskbar handle. `state.taskbar_hwnd` is only set by
            // the legacy cross-taskbar drop path and is otherwise never
            // populated, so it can't be relied on here.
            let taskbar_hwnd = GetParent(hwnd).ok().filter(|h| !h.is_invalid());
            let previous_offset = match lock_state().as_ref() {
                Some(s) => s.tray_offset,
                None => return LRESULT(0),
            };
            let (taskbar_left, max_offset) = taskbar_hwnd
                .and_then(|taskbar| {
                    native_interop::get_taskbar_rect(taskbar).map(|rect| {
                        let tray_left = tray_left_for_taskbar(taskbar, rect);
                        (rect.left, (tray_left - rect.left - widget_width).max(0))
                    })
                })
                .unwrap_or((pt.x, previous_offset.max(0)));
            if let Some(s) = lock_state().as_mut() {
                s.mouse_button_down = true;
                s.dragging = false;
                s.drag_start_mouse_x = pt.x;
                s.drag_start_client_x = pt.x - taskbar_left;
                s.drag_start_offset = previous_offset.clamp(0, max_offset);
                s.drag_max_offset = max_offset;
                s.drag_reposition_pending = false;
            }
            SetCapture(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let threshold = GetSystemMetrics(SM_CXDRAG).max(2);
            let should_post = lock_state().as_mut().is_some_and(|s| {
                if !s.mouse_button_down {
                    return false;
                }
                if !s.dragging {
                    s.dragging = (pt.x - s.drag_start_mouse_x).abs() >= threshold;
                }
                if !s.dragging {
                    return false;
                }
                maybe_retarget_drag_display(s, hwnd, pt);
                s.tray_offset = drag_offset_for_cursor(
                    s.drag_start_offset,
                    s.drag_start_mouse_x,
                    pt.x,
                    s.drag_max_offset,
                );
                if s.drag_reposition_pending {
                    false
                } else {
                    s.drag_reposition_pending = true;
                    true
                }
            });
            if should_post {
                let _ = PostMessageW(hwnd, WM_APP_DRAG_REPOSITION, WPARAM(0), LPARAM(0));
            } else if !lock_state().as_ref().is_some_and(|s| s.dragging) {
                update_mouse_hover(hwnd, lparam);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            clear_mouse_hover(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            if let Some((surface, object)) = mouse_target_at(hwnd, lparam) {
                dispatch_double_click(hwnd, surface, object);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            if let Some((surface, object)) = mouse_target_at(hwnd, lparam) {
                let _ = dispatch_mouse_event(surface, &object, MouseEventKind::RightClick);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (suppressed, was_dragging) = {
                let mut state = lock_state();
                state.as_mut().map_or((false, false), |state| {
                    let suppressed = state.suppress_next_left_up;
                    state.suppress_next_left_up = false;
                    let was_dragging = state.dragging;
                    state.mouse_button_down = false;
                    state.dragging = false;
                    state.drag_reposition_pending = false;
                    (suppressed, was_dragging)
                })
            };
            let _ = ReleaseCapture();
            if suppressed {
                return LRESULT(0);
            }
            // maybe_retarget_drag_display (called from WM_MOUSEMOVE) hands the
            // widget off to whichever monitor's taskbar the cursor crosses
            // onto, so tray_display_override already reflects the drop target.
            if was_dragging {
                save_state_settings();
            } else if let Some((surface, object)) = mouse_target_at(hwnd, lparam) {
                schedule_or_dispatch_click(hwnd, surface, object);
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED | WM_CANCELMODE => {
            if let Some(state) = lock_state().as_mut() {
                state.mouse_button_down = false;
                state.dragging = false;
                state.drag_reposition_pending = false;
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 as u16;
            match id {
                IDM_DASHBOARD => {
                    crate::dashboard::show(hwnd);
                }
                1 => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.force_notify_auth_error = true;
                        }
                    }
                    render_layered();
                    request_poll(hwnd);
                }
                IDM_VERSION_ACTION => {
                    let (install_channel, release) = {
                        let state = lock_state();
                        match state.as_ref() {
                            Some(s) => (
                                s.install_channel,
                                match &s.update_status {
                                    UpdateStatus::Available(release) => Some(release.clone()),
                                    _ => None,
                                },
                            ),
                            None => (InstallChannel::Portable, None),
                        }
                    };

                    match install_channel {
                        InstallChannel::Winget => {
                            if release.is_some() {
                                begin_winget_update(hwnd);
                            } else {
                                begin_update_check(hwnd, true);
                            }
                        }
                        InstallChannel::Portable => {
                            if let Some(release) = release {
                                begin_update_apply(hwnd, release);
                            } else {
                                begin_update_check(hwnd, true);
                            }
                        }
                    }
                }
                2 => {
                    crate::dashboard::close_existing();
                    let _ = DestroyWindow(hwnd);
                }
                IDM_START_WITH_WINDOWS => {
                    set_startup_enabled(!is_startup_enabled());
                }
                IDM_FREQ_1MIN | IDM_FREQ_5MIN | IDM_FREQ_15MIN | IDM_FREQ_1HOUR => {
                    let new_interval = match id {
                        IDM_FREQ_1MIN => POLL_1_MIN,
                        IDM_FREQ_5MIN => POLL_5_MIN,
                        IDM_FREQ_15MIN => POLL_15_MIN,
                        IDM_FREQ_1HOUR => POLL_1_HOUR,
                        _ => POLL_15_MIN,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.poll_interval_ms = new_interval;
                        }
                    }
                    save_state_settings();
                    // Reset the poll timer with the new interval
                    SetTimer(hwnd, TIMER_POLL, new_interval, None);
                }
                id if ProviderId::from_native_menu_command_id(id).is_some() => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            let provider = ProviderId::from_native_menu_command_id(id)
                                .expect("provider menu command was matched above");
                            s.providers.toggle(provider);
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                    sync_tray_icon(hwnd);
                    request_poll(hwnd);
                }
                id if id == IDM_LANG_SYSTEM || language_from_menu_command_id(id).is_some() => {
                    let language_override = if id == IDM_LANG_SYSTEM {
                        None
                    } else {
                        language_from_menu_command_id(id)
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            apply_language_to_state(s, language_override);
                        }
                    }
                    save_state_settings();
                    render_layered();
                }
                _ => {}
            }
            LRESULT(0)
        }
        _ if msg == WM_APP_TRAY => {
            let tray_message = lparam.0 as u32;
            if let Some(surface_index) = tray_icon::themed_surface_index(wparam.0 as u32) {
                let root_id = lock_state().as_ref().and_then(|state| {
                    state
                        .active_theme
                        .as_ref()
                        .and_then(|theme| theme.surfaces.get(surface_index))
                        .map(|surface| surface.id.clone())
                });
                if let Some(root_id) = root_id {
                    match tray_message {
                        WM_MOUSEMOVE => {
                            update_tray_mouse_hover(hwnd, surface_index, root_id);
                            return LRESULT(0);
                        }
                        WM_LBUTTONUP => {
                            let suppressed = {
                                let mut state = lock_state();
                                state.as_mut().is_some_and(|state| {
                                    let suppressed = state.suppress_next_left_up;
                                    state.suppress_next_left_up = false;
                                    suppressed
                                })
                            };
                            if suppressed {
                                return LRESULT(0);
                            }
                            if mouse_handler_exists(surface_index, &root_id, MouseEventKind::Click)
                            {
                                schedule_or_dispatch_click(hwnd, surface_index, root_id);
                            } else if !mouse_handler_exists(
                                surface_index,
                                &root_id,
                                MouseEventKind::DoubleClick,
                            ) {
                                crate::dashboard::show(hwnd);
                            }
                            return LRESULT(0);
                        }
                        WM_LBUTTONDBLCLK => {
                            if mouse_handler_exists(
                                surface_index,
                                &root_id,
                                MouseEventKind::DoubleClick,
                            ) {
                                dispatch_double_click(hwnd, surface_index, root_id);
                            } else {
                                crate::dashboard::show(hwnd);
                            }
                            return LRESULT(0);
                        }
                        WM_RBUTTONUP | WM_CONTEXTMENU => {
                            if !dispatch_mouse_event(
                                surface_index,
                                &root_id,
                                MouseEventKind::RightClick,
                            ) {
                                show_context_menu_document(hwnd, None, None);
                            }
                            return LRESULT(0);
                        }
                        _ => {}
                    }
                }
            }
            match tray_icon::handle_message(lparam) {
                tray_icon::TrayAction::OpenDashboard => {
                    crate::dashboard::show(hwnd);
                }
                tray_icon::TrayAction::ShowContextMenu => {
                    show_context_menu_document(hwnd, None, None);
                }
                tray_icon::TrayAction::None => {}
            }
            LRESULT(0)
        }
        _ if msg == taskbar_created_message() => {
            // Explorer discards notification icons when it restarts. Floating
            // and tray-icon-only themes keep their owner HWND, so restore the
            // registrations when the shell broadcasts its return.
            sync_tray_icon(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            crate::dashboard::close_existing();
            crate::desktop_compositor::clear();
            let desktop_windows = {
                let mut state = lock_state();
                match state.as_mut() {
                    Some(state) => std::mem::take(&mut state.desktop_hwnds),
                    None => Vec::new(),
                }
            };
            for window in desktop_windows.into_iter().flatten() {
                let _ = DestroyWindow(window.to_hwnd());
            }
            tray_icon::remove_all(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
