#[cfg(not(windows))]
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("the status guardian is only available on Windows")
}

#[cfg(not(windows))]
pub fn restart_installed(_executable: &std::path::Path) -> anyhow::Result<()> {
    anyhow::bail!("the status guardian is only available on Windows")
}

#[cfg(not(windows))]
pub fn ensure_started() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
pub fn stop_existing(_timeout: std::time::Duration) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(windows)]
mod windows {
    use std::{
        mem::{size_of, zeroed},
        path::Path,
        process::{Command, Stdio},
        ptr::{null, null_mut},
        thread,
        time::{Duration, Instant},
    };

    use anyhow::{bail, Context, Result};
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HINSTANCE, HWND, LPARAM,
            LRESULT, POINT, WPARAM,
        },
        System::{
            LibraryLoader::GetModuleHandleW,
            Threading::{
                CreateMutexW, CreateProcessW, OpenMutexW, ReleaseMutex, CREATE_BREAKAWAY_FROM_JOB,
                CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, MUTEX_MODIFY_STATE,
                PROCESS_INFORMATION, STARTUPINFOW,
            },
        },
        UI::{
            Shell::{
                ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP,
                NIF_TIP, NIIF_INFO, NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
                NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DestroyWindow, DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW,
                GetWindowLongPtrW, KillTimer, LoadCursorW, LoadIconW, PostMessageW,
                PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
                SetTimer, SetWindowLongPtrW, TrackPopupMenu, TranslateMessage, CS_HREDRAW,
                CS_VREDRAW, GWLP_USERDATA, HICON, HMENU, IDC_ARROW, MF_GRAYED, MF_SEPARATOR,
                MF_STRING, MSG, SW_HIDE, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE,
                WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_NULL, WM_TIMER,
                WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
            },
        },
    };

    use crate::{
        diagnostics,
        install::{self, RuntimeState, StatusReport},
        model_config,
        runtime::{RuntimeKind, RuntimeRegistration},
    };

    const WINDOW_CLASS: &str = "CodexImageFixGuardianWindowV1";
    const MUTEX_NAME: &str = "Local\\comidea.CodexImageFix.Guardian";
    const TRAY_CALLBACK: u32 = WM_APP + 80;
    const TRAY_ID: u32 = 1;
    const TIMER_ID: usize = 1;
    const TIMER_INTERVAL_MS: u32 = 3_000;
    const ID_OPEN: usize = 1;
    const ID_REPAIR: usize = 2;
    const ID_LAUNCH_CODEX: usize = 3;
    const ID_EXIT: usize = 4;
    const GUARDIAN_CREATION_FLAGS: u32 =
        CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum VisualState {
        Green,
        Gray,
        Amber,
        Red,
    }

    struct Icons {
        green: HICON,
        gray: HICON,
        amber: HICON,
        red: HICON,
    }

    impl Icons {
        unsafe fn load(instance: HINSTANCE) -> Result<Self> {
            let icons = Self {
                green: LoadIconW(instance, int_resource(2)),
                gray: LoadIconW(instance, int_resource(3)),
                amber: LoadIconW(instance, int_resource(4)),
                red: LoadIconW(instance, int_resource(5)),
            };
            if [icons.green, icons.gray, icons.amber, icons.red]
                .into_iter()
                .any(|icon| icon.is_null())
            {
                bail!("failed to load status icons");
            }
            Ok(icons)
        }

        fn get(&self, state: VisualState) -> HICON {
            match state {
                VisualState::Green => self.green,
                VisualState::Gray => self.gray,
                VisualState::Amber => self.amber,
                VisualState::Red => self.red,
            }
        }
    }

    struct GuardianState {
        registration: RuntimeRegistration,
        icons: Icons,
        visual: VisualState,
        report: Option<StatusReport>,
        taskbar_created: u32,
    }

    struct GuardianMutex(HANDLE);

    impl GuardianMutex {
        fn acquire() -> Result<Option<Self>> {
            Self::acquire_named(MUTEX_NAME)
        }

        fn acquire_named(name: &str) -> Result<Option<Self>> {
            let name = wide(name);
            let handle = unsafe { CreateMutexW(null(), 1, name.as_ptr()) };
            if handle.is_null() {
                return Err(std::io::Error::last_os_error())
                    .context("failed to create guardian mutex");
            }
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                unsafe { CloseHandle(handle) };
                return Ok(None);
            }
            Ok(Some(Self(handle)))
        }
    }

    impl Drop for GuardianMutex {
        fn drop(&mut self) {
            unsafe {
                ReleaseMutex(self.0);
                CloseHandle(self.0);
            }
        }
    }

    pub fn run() -> Result<()> {
        let Some(_mutex) = GuardianMutex::acquire()? else {
            return Ok(());
        };
        unsafe {
            let instance = GetModuleHandleW(null());
            let class_name = wide(WINDOW_CLASS);
            let window_class = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(window_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: LoadIconW(instance, int_resource(1)),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: null_mut(),
                lpszMenuName: null(),
                lpszClassName: class_name.as_ptr(),
            };
            if RegisterClassW(&window_class) == 0 {
                bail!("failed to register guardian window class");
            }
            let window = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class_name.as_ptr(),
                wide("Comidea Codex Image Bridge Guardian").as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null_mut(),
            );
            if window.is_null() {
                bail!("failed to create guardian window");
            }
            let state = Box::new(GuardianState {
                registration: RuntimeRegistration::register(RuntimeKind::Guardian)?,
                icons: Icons::load(instance)?,
                visual: VisualState::Gray,
                report: None,
                taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            });
            SetWindowLongPtrW(window, GWLP_USERDATA, Box::into_raw(state) as isize);
            add_tray_icon(window)?;
            if SetTimer(window, TIMER_ID, TIMER_INTERVAL_MS, None) == 0 {
                DestroyWindow(window);
                bail!("failed to start guardian timer");
            }
            refresh(window, true);

            let mut message: MSG = zeroed();
            while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        Ok(())
    }

    pub fn restart_installed(executable: &Path) -> Result<()> {
        stop_existing(Duration::from_secs(5))?;
        start_detached(executable)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if crate::runtime::active_runtime(executable)
                .is_ok_and(|runtime| !runtime.guardian_pids.is_empty())
            {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
        bail!("status guardian did not become ready")
    }

    pub fn ensure_started() -> Result<()> {
        let executable = std::env::current_exe().context("failed to locate guardian executable")?;
        ensure_started_with(guardian_is_running, || start_detached(&executable)).map(|_| ())
    }

    fn ensure_started_with(
        is_running: impl FnOnce() -> bool,
        start: impl FnOnce() -> Result<()>,
    ) -> Result<bool> {
        if is_running() {
            return Ok(false);
        }
        start()?;
        Ok(true)
    }

    fn guardian_is_running() -> bool {
        let name = wide(MUTEX_NAME);
        let handle = unsafe { OpenMutexW(MUTEX_MODIFY_STATE, 0, name.as_ptr()) };
        if handle.is_null() {
            return false;
        }
        unsafe { CloseHandle(handle) };
        true
    }

    fn start_detached(executable: &Path) -> Result<()> {
        match start_with_create_process(executable) {
            Ok(()) => Ok(()),
            Err(create_error) => start_via_shell(executable).with_context(|| {
                format!("breakaway launch failed ({create_error}); shell fallback also failed")
            }),
        }
    }

    fn start_with_create_process(executable: &Path) -> Result<()> {
        use std::os::windows::ffi::OsStrExt;

        let application = executable
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut command_line = format!("\"{}\" guardian", executable.display())
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        let succeeded = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                0,
                GUARDIAN_CREATION_FLAGS,
                null(),
                null(),
                &startup,
                &mut process,
            )
        };
        if succeeded == 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to start guardian at {}", executable.display()));
        }
        unsafe {
            CloseHandle(process.hThread);
            CloseHandle(process.hProcess);
        }
        Ok(())
    }

    fn start_via_shell(executable: &Path) -> Result<()> {
        let operation = wide("open");
        let executable = wide(&executable.to_string_lossy());
        let arguments = wide("guardian");
        let result = unsafe {
            ShellExecuteW(
                null_mut(),
                operation.as_ptr(),
                executable.as_ptr(),
                arguments.as_ptr(),
                null(),
                SW_HIDE,
            )
        } as isize;
        if result <= 32 {
            bail!("ShellExecuteW returned error code {result}");
        }
        Ok(())
    }

    pub fn stop_existing(timeout: Duration) -> Result<()> {
        unsafe {
            let class_name = wide(WINDOW_CLASS);
            let window = FindWindowW(class_name.as_ptr(), null());
            if window.is_null() {
                return Ok(());
            }
            if PostMessageW(window, WM_CLOSE, 0, 0) == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("failed to stop status guardian");
            }
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if FindWindowW(class_name.as_ptr(), null()).is_null() {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
        bail!("status guardian did not exit in time")
    }

    unsafe extern "system" fn window_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if state(window)
            .is_some_and(|state| state.taskbar_created != 0 && message == state.taskbar_created)
        {
            let _ = add_tray_icon(window);
            return 0;
        }
        match message {
            WM_TIMER if wparam == TIMER_ID => {
                refresh(window, false);
                0
            }
            TRAY_CALLBACK => {
                let event = (lparam as u32) & 0xffff;
                if matches!(event, WM_CONTEXTMENU) {
                    show_menu(window);
                } else if matches!(event, WM_LBUTTONDBLCLK | NIN_SELECT) {
                    open_control_panel(window);
                }
                0
            }
            WM_COMMAND => {
                handle_menu_command(window, wparam & 0xffff);
                0
            }
            WM_CLOSE => {
                DestroyWindow(window);
                0
            }
            WM_DESTROY => {
                KillTimer(window, TIMER_ID);
                delete_tray_icon(window);
                let pointer = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut GuardianState;
                if !pointer.is_null() {
                    drop(Box::from_raw(pointer));
                    SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                }
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }

    unsafe fn refresh(window: HWND, initial: bool) {
        let Some(state) = state_mut(window) else {
            return;
        };
        state.registration.heartbeat();
        let (repaired, report) = match repair_then_status(
            install::repair_missing_integration_entries,
            install::status_report,
        ) {
            Ok(result) => result,
            Err(_) => return,
        };
        if !report.codex_running {
            let _ = model_config::ensure_advanced_model_picker();
        }
        let next = visual_state(&report);
        let previous = state.visual;
        state.visual = next;
        state.report = Some(report.clone());
        let _ = update_tray_icon(window, next, status_text(&report));
        if repaired {
            if report.restart_required {
                notify(
                    window,
                    "图片代理入口已恢复",
                    "请完全退出并重新启动 Codex，当前会话才能重新接入图片代理。",
                    NIIF_WARNING,
                );
            } else {
                notify(
                    window,
                    "图片代理入口已恢复",
                    "后续启动 Codex 时会自动接入图片代理。",
                    NIIF_INFO,
                );
            }
        } else if next == VisualState::Red && (initial || previous != VisualState::Red) {
            notify(
                window,
                "图片代理需要处理",
                status_text(&report),
                NIIF_WARNING,
            );
        }
    }

    fn repair_then_status(
        repair: impl FnOnce() -> Result<bool>,
        status: impl FnOnce() -> Result<StatusReport>,
    ) -> Result<(bool, StatusReport)> {
        let repaired = repair().unwrap_or(false);
        Ok((repaired, status()?))
    }

    unsafe fn show_menu(window: HWND) {
        let Some(state) = state(window) else {
            return;
        };
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let current = state
            .report
            .as_ref()
            .map(status_text)
            .unwrap_or("正在读取状态");
        append_menu(
            menu,
            MF_STRING | MF_GRAYED,
            0,
            &format!("当前状态：{current}"),
        );
        append_menu(menu, MF_SEPARATOR, 0, "");
        append_menu(menu, MF_STRING, ID_OPEN, "打开控制面板");
        append_menu(menu, MF_STRING, ID_REPAIR, "立即修复");
        append_menu(menu, MF_STRING, ID_LAUNCH_CODEX, "启动 Codex");
        append_menu(menu, MF_SEPARATOR, 0, "");
        append_menu(menu, MF_STRING, ID_EXIT, "退出状态监控");
        let mut point = POINT::default();
        GetCursorPos(&mut point);
        SetForegroundWindow(window);
        let command = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD,
            point.x,
            point.y,
            0,
            window,
            null(),
        ) as usize;
        DestroyMenu(menu);
        PostMessageW(window, WM_NULL, 0, 0);
        if command != 0 {
            handle_menu_command(window, command);
        }
    }

    unsafe fn append_menu(menu: HMENU, flags: u32, id: usize, text: &str) {
        let label = wide(text);
        AppendMenuW(menu, flags, id, label.as_ptr());
    }

    unsafe fn handle_menu_command(window: HWND, command: usize) {
        match command {
            ID_OPEN => open_control_panel(window),
            ID_REPAIR => match install::repair_missing_integration_entries() {
                Ok(true) => refresh(window, false),
                Ok(false) => notify(
                    window,
                    "无需修复",
                    "CODEX_CLI_PATH 已正确指向图片代理入口。",
                    NIIF_INFO,
                ),
                Err(error) => {
                    notify(window, "无法自动修复", &short_error(&error), NIIF_WARNING);
                    open_control_panel(window);
                }
            },
            ID_LAUNCH_CODEX => match diagnostics::launch_codex() {
                Ok(()) => notify(window, "Codex", "已提交启动请求。", NIIF_INFO),
                Err(error) => notify(window, "无法启动 Codex", &short_error(&error), NIIF_WARNING),
            },
            ID_EXIT => {
                DestroyWindow(window);
            }
            _ => {}
        }
    }

    unsafe fn open_control_panel(window: HWND) {
        match std::env::current_exe()
            .context("failed to locate current executable")
            .and_then(|executable| {
                Command::new(executable)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .context("failed to open control panel")
                    .map(|_| ())
            }) {
            Ok(()) => {}
            Err(error) => notify(
                window,
                "无法打开控制面板",
                &short_error(&error),
                NIIF_WARNING,
            ),
        }
    }

    unsafe fn add_tray_icon(window: HWND) -> Result<()> {
        let state = state(window).context("guardian state is unavailable")?;
        let mut data = notify_data(
            window,
            state.icons.get(state.visual),
            state
                .report
                .as_ref()
                .map(status_text)
                .unwrap_or("正在读取状态"),
        );
        if Shell_NotifyIconW(NIM_ADD, &data) == 0 {
            return Err(std::io::Error::last_os_error()).context("failed to add tray icon");
        }
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &data);
        Ok(())
    }

    unsafe fn update_tray_icon(window: HWND, visual: VisualState, text: &str) -> Result<()> {
        let state = state(window).context("guardian state is unavailable")?;
        let data = notify_data(window, state.icons.get(visual), text);
        if Shell_NotifyIconW(NIM_MODIFY, &data) == 0 {
            return Err(std::io::Error::last_os_error()).context("failed to update tray icon");
        }
        Ok(())
    }

    unsafe fn delete_tray_icon(window: HWND) {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            uID: TRAY_ID,
            ..Default::default()
        };
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_DELETE, &data);
    }

    unsafe fn notify(window: HWND, title: &str, message: &str, tone: u32) {
        let Some(state) = state(window) else {
            return;
        };
        let mut data = notify_data(
            window,
            state.icons.get(state.visual),
            status_text_opt(state),
        );
        data.uFlags |= NIF_INFO;
        copy_wide(&mut data.szInfoTitle, title);
        copy_wide(&mut data.szInfo, message);
        data.dwInfoFlags = tone;
        Shell_NotifyIconW(NIM_MODIFY, &data);
    }

    fn notify_data(window: HWND, icon: HICON, tip: &str) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            uID: TRAY_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: TRAY_CALLBACK,
            hIcon: icon,
            ..Default::default()
        };
        copy_wide(
            &mut data.szTip,
            &format!("Comidea Codex Image Bridge - {tip}"),
        );
        data
    }

    fn visual_state(report: &StatusReport) -> VisualState {
        match report.runtime_state {
            RuntimeState::Connected => VisualState::Green,
            RuntimeState::Ready => VisualState::Gray,
            RuntimeState::RestartRequired => VisualState::Amber,
            RuntimeState::NotInstalled | RuntimeState::Broken => VisualState::Red,
        }
    }

    fn status_text(report: &StatusReport) -> &'static str {
        match report.runtime_state {
            RuntimeState::Connected => "图片代理已连接",
            RuntimeState::Ready => "入口已就绪，Codex 未运行",
            RuntimeState::RestartRequired => "入口已修复，请重启 Codex",
            RuntimeState::NotInstalled => "图片代理尚未安装",
            RuntimeState::Broken if report.codex_running && !report.proxy_running => {
                "Codex 未连接图片代理"
            }
            RuntimeState::Broken => "安装或入口状态异常",
        }
    }

    fn status_text_opt(state: &GuardianState) -> &'static str {
        state
            .report
            .as_ref()
            .map(status_text)
            .unwrap_or("正在读取状态")
    }

    fn short_error(error: &anyhow::Error) -> String {
        let message = format!("{error:#}");
        message.chars().take(220).collect()
    }

    unsafe fn state(window: HWND) -> Option<&'static GuardianState> {
        (GetWindowLongPtrW(window, GWLP_USERDATA) as *const GuardianState).as_ref()
    }

    unsafe fn state_mut(window: HWND) -> Option<&'static mut GuardianState> {
        (GetWindowLongPtrW(window, GWLP_USERDATA) as *mut GuardianState).as_mut()
    }

    fn copy_wide<const N: usize>(destination: &mut [u16; N], value: &str) {
        destination.fill(0);
        for (slot, character) in destination
            .iter_mut()
            .take(N.saturating_sub(1))
            .zip(value.encode_utf16())
        {
            *slot = character;
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn int_resource(id: usize) -> *const u16 {
        std::ptr::without_provenance(id)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn report(runtime_state: RuntimeState) -> StatusReport {
            StatusReport {
                health: install::InstallationHealth::Healthy,
                fix_root: "fix".into(),
                state_present: true,
                launcher_present: true,
                proxy_present: true,
                alias_present: true,
                codex_cli_path: Some("launcher".to_owned()),
                real_cli: Some("codex".into()),
                real_cli_error: None,
                environment_healthy: true,
                alias_healthy: true,
                proxy_healthy: true,
                launcher_healthy: true,
                guardian_installed: true,
                guardian_running: true,
                codex_running: false,
                proxy_running: false,
                restart_required: false,
                runtime_state,
            }
        }

        #[test]
        fn runtime_states_map_to_distinct_tray_colors() {
            assert_eq!(
                visual_state(&report(RuntimeState::Connected)),
                VisualState::Green
            );
            assert_eq!(
                visual_state(&report(RuntimeState::Ready)),
                VisualState::Gray
            );
            assert_eq!(
                visual_state(&report(RuntimeState::RestartRequired)),
                VisualState::Amber
            );
            assert_eq!(
                visual_state(&report(RuntimeState::Broken)),
                VisualState::Red
            );
        }

        #[test]
        fn guardian_mutex_allows_only_one_instance() {
            let name = format!(
                "Local\\comidea.CodexImageFix.Guardian.Test.{}",
                std::process::id()
            );
            let first = GuardianMutex::acquire_named(&name).unwrap().unwrap();
            assert!(GuardianMutex::acquire_named(&name).unwrap().is_none());
            drop(first);
            assert!(GuardianMutex::acquire_named(&name).unwrap().is_some());
        }

        #[test]
        fn ensure_started_skips_launch_when_guardian_exists() {
            let mut launched = false;
            let started = ensure_started_with(
                || true,
                || {
                    launched = true;
                    Ok(())
                },
            )
            .unwrap();
            assert!(!started);
            assert!(!launched);
        }

        #[test]
        fn ensure_started_propagates_launch_failure_without_waiting() {
            let error =
                ensure_started_with(|| false, || anyhow::bail!("launch failed")).unwrap_err();
            assert!(error.to_string().contains("launch failed"));
        }

        #[test]
        fn guardian_launch_breaks_away_without_inheriting_a_console() {
            assert_ne!(GUARDIAN_CREATION_FLAGS & CREATE_BREAKAWAY_FROM_JOB, 0);
            assert_ne!(GUARDIAN_CREATION_FLAGS & CREATE_NEW_PROCESS_GROUP, 0);
            assert_ne!(GUARDIAN_CREATION_FLAGS & CREATE_NO_WINDOW, 0);
        }

        #[test]
        fn guardian_repairs_entries_before_reading_status() {
            let phase = std::cell::Cell::new(0);
            let (repaired, _) = repair_then_status(
                || {
                    assert_eq!(phase.get(), 0);
                    phase.set(1);
                    Ok(true)
                },
                || {
                    assert_eq!(phase.get(), 1);
                    phase.set(2);
                    Ok(report(RuntimeState::Ready))
                },
            )
            .unwrap();
            assert!(repaired);
            assert_eq!(phase.get(), 2);
        }
    }
}

#[cfg(windows)]
pub use windows::{ensure_started, restart_installed, run, stop_existing};
