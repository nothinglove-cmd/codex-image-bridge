#[cfg(not(windows))]
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("the graphical installer is only available on Windows")
}

#[cfg(windows)]
mod windows {
    use std::{
        mem::zeroed,
        ptr::{null, null_mut},
        thread,
    };

    use anyhow::{bail, Result};
    use windows_sys::Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
            CreateSolidBrush, DeleteDC, DeleteObject, DrawFocusRect, DrawTextW, Ellipse, EndPaint,
            FillRect, GetSysColorBrush, InvalidateRect, RedrawWindow, RoundRect, SelectObject,
            SetBkColor, SetBkMode, SetTextColor, UpdateWindow, CLEARTYPE_QUALITY,
            CLIP_DEFAULT_PRECIS, COLOR_WINDOW, DEFAULT_CHARSET, DEFAULT_PITCH, DT_CENTER,
            DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FW_BOLD, FW_NORMAL,
            HBRUSH, HDC, HFONT, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID, RDW_ALLCHILDREN,
            RDW_INVALIDATE, RDW_UPDATENOW, SRCCOPY, TRANSPARENT,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::{
                DRAWITEMSTRUCT, EM_SETMARGINS, EM_SETPASSWORDCHAR, ODS_DISABLED, ODS_FOCUS,
                ODS_SELECTED,
            },
            HiDpi::{
                GetDpiForSystem, SetProcessDpiAwarenessContext,
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            },
            Input::KeyboardAndMouse::{EnableWindow, GetFocus, SetFocus},
            WindowsAndMessaging::{
                BeginDeferWindowPos, CreateWindowExW, DefWindowProcW, DeferWindowPos,
                DestroyWindow, DispatchMessageW, EndDeferWindowPos, GetClientRect, GetMessageW,
                GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, LoadCursorW, LoadIconW,
                MessageBoxW, MoveWindow, PostMessageW, PostQuitMessage, RegisterClassW,
                SendMessageW, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow,
                SystemParametersInfoW, TranslateMessage, BN_CLICKED, BS_OWNERDRAW, CS_HREDRAW,
                CS_VREDRAW, EC_LEFTMARGIN, EC_RIGHTMARGIN, ES_AUTOHSCROLL, ES_AUTOVSCROLL,
                ES_MULTILINE, ES_PASSWORD, ES_READONLY, GWLP_USERDATA, IDC_ARROW, IDI_APPLICATION,
                IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_YESNO,
                MINMAXINFO, MSG, SPI_GETWORKAREA, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE,
                SWP_NOREDRAW, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, SW_SHOW, WM_APP,
                WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_DESTROY,
                WM_DPICHANGED, WM_DRAWITEM, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_PAINT, WM_SETFONT,
                WM_SIZE, WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_CLIENTEDGE,
                WS_EX_CONTROLPARENT, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
            },
        },
    };
    use zeroize::Zeroizing;

    use crate::{
        diagnostics,
        install::{self, InstallationHealth, StatusReport},
        model_config::{self, ConnectionReport, ModelConfiguration, ModelRevisions, ModelSettings},
    };

    const WINDOW_TITLE: &str = "Comidea Codex Image Bridge";
    const WINDOW_CLASS: &str = "CodexImageFixWindowV2";
    const WM_OPERATION_COMPLETE: u32 = WM_APP + 41;
    const WM_APPLY_PAGE: u32 = WM_APP + 42;

    const ID_NAV_INSTALL: i32 = 1001;
    const ID_NAV_MODEL: i32 = 1002;
    const ID_NAV_DIAGNOSTICS: i32 = 1003;
    const ID_INSTALL: i32 = 2001;
    const ID_UNINSTALL: i32 = 2002;
    const ID_INSTALL_REFRESH: i32 = 2003;
    const ID_SERVER_URL: i32 = 3001;
    const ID_API_KEY: i32 = 3002;
    const ID_TOGGLE_KEY: i32 = 3003;
    const ID_TEST_CONNECTION: i32 = 3004;
    const ID_MODEL_TOGGLE: i32 = 3005;
    const ID_RESTORE_CONFIG: i32 = 3006;
    const ID_SAVE_CONFIG: i32 = 3007;
    const ID_IMAGE_MODEL: i32 = 3008;
    const ID_STATIC_HEADERS: i32 = 3009;
    const ID_ENV_HEADERS: i32 = 3010;
    const ID_DIAGNOSTICS_REFRESH: i32 = 4001;
    const ID_DIAGNOSTICS_EXPORT: i32 = 4002;
    const ID_LAUNCH_CODEX: i32 = 4003;

    const COLOR_SIDEBAR: COLORREF = rgb(241, 243, 246);
    const COLOR_ACTIONBAR: COLORREF = rgb(248, 249, 251);
    const COLOR_WHITE: COLORREF = rgb(255, 255, 255);
    const COLOR_INPUT: COLORREF = rgb(251, 252, 253);
    const COLOR_CARD: COLORREF = rgb(250, 251, 252);
    const COLOR_BORDER: COLORREF = rgb(215, 221, 228);
    const COLOR_BORDER_DARK: COLORREF = rgb(199, 206, 215);
    const COLOR_TEXT: COLORREF = rgb(23, 32, 42);
    const COLOR_BODY: COLORREF = rgb(70, 81, 94);
    const COLOR_MUTED: COLORREF = rgb(109, 119, 131);
    const COLOR_TEAL: COLORREF = rgb(18, 110, 135);
    const COLOR_TEAL_DARK: COLORREF = rgb(16, 95, 117);
    const COLOR_GREEN: COLORREF = rgb(31, 124, 80);
    const COLOR_GREEN_PALE: COLORREF = rgb(223, 242, 231);
    const COLOR_RED: COLORREF = rgb(190, 45, 45);
    const COLOR_AMBER: COLORREF = rgb(165, 115, 8);

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Page {
        Install,
        Model,
        Diagnostics,
    }

    #[derive(Default)]
    struct PageSwitchQueue {
        target: Option<Page>,
        message_pending: bool,
    }

    impl PageSwitchQueue {
        fn request(&mut self, page: Page) -> bool {
            self.target = Some(page);
            if self.message_pending {
                false
            } else {
                self.message_pending = true;
                true
            }
        }

        fn take(&mut self) -> Option<Page> {
            self.message_pending = false;
            self.target.take()
        }
    }

    enum Operation {
        Refresh,
        Install,
        Uninstall,
        TestConnection(ModelConfiguration),
        SaveModel {
            configuration: ModelConfiguration,
            revisions: ModelRevisions,
        },
        RestoreModel,
        ExportDiagnostics,
        LaunchCodex,
    }

    enum OperationResult {
        Refresh {
            report: std::result::Result<StatusReport, String>,
            settings: std::result::Result<ModelSettings, String>,
        },
        Install {
            action: std::result::Result<(), String>,
            report: std::result::Result<StatusReport, String>,
        },
        Uninstall {
            action: std::result::Result<install::UninstallOutcome, String>,
            report: std::result::Result<StatusReport, String>,
            settings: std::result::Result<ModelSettings, String>,
        },
        TestConnection(std::result::Result<ConnectionReport, String>),
        SaveModel {
            action: std::result::Result<(), String>,
            settings: Option<std::result::Result<ModelSettings, String>>,
        },
        RestoreModel {
            action: std::result::Result<bool, String>,
            settings: Option<std::result::Result<ModelSettings, String>>,
        },
        ExportDiagnostics(std::result::Result<std::path::PathBuf, String>),
        LaunchCodex(std::result::Result<(), String>),
    }

    struct Controls {
        nav_install: HWND,
        nav_model: HWND,
        nav_diagnostics: HWND,
        install_details: HWND,
        install: HWND,
        uninstall: HWND,
        install_refresh: HWND,
        server_url: HWND,
        api_key: HWND,
        toggle_key: HWND,
        test_connection: HWND,
        image_model: HWND,
        model_toggle: HWND,
        static_headers: HWND,
        env_headers: HWND,
        restore_config: HWND,
        save_config: HWND,
        diagnostics_details: HWND,
        diagnostics_refresh: HWND,
        diagnostics_export: HWND,
        launch_codex: HWND,
    }

    struct Fonts {
        title: HFONT,
        heading: HFONT,
        body: HFONT,
        body_bold: HFONT,
        small: HFONT,
        small_bold: HFONT,
        mono: HFONT,
    }

    #[derive(Clone, Copy)]
    enum MessageTone {
        Good,
        Error,
        Muted,
    }

    struct UiState {
        controls: Controls,
        fonts: Fonts,
        input_brush: HBRUSH,
        details_brush: HBRUSH,
        dpi: u32,
        layout_dpi: u32,
        page: Page,
        page_controls_initialized: bool,
        page_switch: PageSwitchQueue,
        busy: bool,
        busy_text: String,
        key_visible: bool,
        model_enabled: bool,
        model_dirty: bool,
        loading_model_controls: bool,
        install_report: Option<StatusReport>,
        install_error: Option<String>,
        model_settings: Option<ModelSettings>,
        model_error: Option<String>,
        connection_message: Option<(String, MessageTone)>,
    }

    pub fn run() -> Result<()> {
        unsafe {
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let instance = GetModuleHandleW(null());
            let class_name = wide(WINDOW_CLASS);
            let app_icon = load_app_icon(instance);
            let window_class = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(window_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: app_icon,
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: GetSysColorBrush(COLOR_WINDOW),
                lpszMenuName: null(),
                lpszClassName: class_name.as_ptr(),
            };
            if RegisterClassW(&window_class) == 0 {
                bail!("failed to register the Windows UI class");
            }

            let dpi = GetDpiForSystem().max(96);
            let mut work_area: RECT = zeroed();
            if SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work_area as *mut RECT).cast(), 0)
                == 0
            {
                work_area = RECT {
                    left: 0,
                    top: 0,
                    right: scale_for(dpi, 1020),
                    bottom: scale_for(dpi, 690),
                };
            }
            let available_width = work_area.right - work_area.left;
            let available_height = work_area.bottom - work_area.top;
            let window_width = scale_for(dpi, 1020)
                .min(available_width - scale_for(dpi, 24))
                .max(available_width.min(860));
            let window_height = scale_for(dpi, 690)
                .min(available_height - scale_for(dpi, 24))
                .max(available_height.min(600));
            let window = CreateWindowExW(
                WS_EX_CONTROLPARENT,
                class_name.as_ptr(),
                wide(WINDOW_TITLE).as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
                work_area.left + (available_width - window_width) / 2,
                work_area.top + (available_height - window_height) / 2,
                window_width,
                window_height,
                null_mut(),
                null_mut(),
                instance,
                null_mut(),
            );
            if window.is_null() {
                bail!("failed to create the Windows UI");
            }

            let state = Box::new(create_state(window, instance, dpi)?);
            SetWindowLongPtrW(window, GWLP_USERDATA, Box::into_raw(state) as isize);
            apply_page(window, Page::Model);
            layout(window);
            ShowWindow(window, SW_SHOW);
            UpdateWindow(window);
            start_operation(window, Operation::Refresh);

            let mut message: MSG = zeroed();
            while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        Ok(())
    }

    pub fn show_fatal_error(message: &str) {
        unsafe {
            MessageBoxW(
                null_mut(),
                wide(message).as_ptr(),
                wide(WINDOW_TITLE).as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }

    unsafe extern "system" fn window_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_CREATE => 0,
            WM_SIZE => {
                layout(window);
                InvalidateRect(window, null(), 0);
                0
            }
            WM_DPICHANGED => {
                let dpi = (wparam & 0xffff) as u32;
                let suggested = &*(lparam as *const RECT);
                update_dpi(window, dpi.max(96), suggested);
                0
            }
            WM_GETMINMAXINFO => {
                let info = lparam as *mut MINMAXINFO;
                if let Some(info) = info.as_mut() {
                    let dpi = state(window).map_or(96, |state| state.dpi);
                    info.ptMinTrackSize.x = scale_for(dpi, 860);
                    info.ptMinTrackSize.y = scale_for(dpi, 600);
                }
                0
            }
            WM_COMMAND => {
                let command = (wparam & 0xffff) as i32;
                let notification = ((wparam >> 16) & 0xffff) as u16;
                if matches!(
                    command,
                    ID_SERVER_URL
                        | ID_API_KEY
                        | ID_IMAGE_MODEL
                        | ID_STATIC_HEADERS
                        | ID_ENV_HEADERS
                ) && matches!(notification, 0x0100 | 0x0200)
                {
                    InvalidateRect(window, null(), 0);
                }
                if matches!(
                    command,
                    ID_SERVER_URL
                        | ID_API_KEY
                        | ID_IMAGE_MODEL
                        | ID_STATIC_HEADERS
                        | ID_ENV_HEADERS
                ) && notification == 0x0300
                {
                    if let Some(state) = state_mut(window) {
                        if !state.loading_model_controls {
                            state.model_dirty = true;
                        }
                    }
                }
                if notification == BN_CLICKED as u16 {
                    handle_command(window, command);
                }
                0
            }
            WM_APPLY_PAGE => {
                apply_pending_page(window);
                0
            }
            WM_DRAWITEM => {
                let item = &*(lparam as *const DRAWITEMSTRUCT);
                draw_button(window, item);
                1
            }
            WM_PAINT => {
                paint_window(window);
                0
            }
            WM_ERASEBKGND => 1,
            WM_CTLCOLOREDIT => {
                let Some(state) = state(window) else {
                    return DefWindowProcW(window, message, wparam, lparam);
                };
                SetBkColor(wparam as HDC, COLOR_INPUT);
                SetTextColor(wparam as HDC, COLOR_TEXT);
                state.input_brush as LRESULT
            }
            WM_CTLCOLORSTATIC => {
                let Some(state) = state(window) else {
                    return DefWindowProcW(window, message, wparam, lparam);
                };
                SetBkColor(wparam as HDC, COLOR_CARD);
                SetTextColor(wparam as HDC, COLOR_BODY);
                state.details_brush as LRESULT
            }
            WM_OPERATION_COMPLETE => {
                let result = Box::from_raw(lparam as *mut OperationResult);
                complete_operation(window, *result);
                0
            }
            WM_CLOSE => {
                if state(window).is_some_and(|state| state.busy) {
                    MessageBoxW(
                        window,
                        wide("当前操作尚未完成，请稍候。").as_ptr(),
                        wide(WINDOW_TITLE).as_ptr(),
                        MB_OK | MB_ICONINFORMATION,
                    );
                } else if state(window).is_some_and(|state| state.model_dirty)
                    && MessageBoxW(
                        window,
                        wide("模型服务配置还有未保存的修改，关闭后这些修改会丢失。继续关闭吗？")
                            .as_ptr(),
                        wide("未保存的修改").as_ptr(),
                        MB_YESNO | MB_ICONWARNING,
                    ) != IDYES
                {
                } else {
                    DestroyWindow(window);
                }
                0
            }
            WM_DESTROY => {
                let pointer = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut UiState;
                if !pointer.is_null() {
                    let state = Box::from_raw(pointer);
                    delete_fonts(&state.fonts);
                    DeleteObject(state.input_brush as _);
                    DeleteObject(state.details_brush as _);
                    SetWindowLongPtrW(window, GWLP_USERDATA, 0);
                }
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }

    unsafe fn create_state(window: HWND, instance: HINSTANCE, dpi: u32) -> Result<UiState> {
        let owner_button =
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_CLIPSIBLINGS | BS_OWNERDRAW as u32;
        let nav_install = create_control(
            0,
            "BUTTON",
            "安装状态",
            owner_button,
            ID_NAV_INSTALL,
            window,
            instance,
        )?;
        let nav_model = create_control(
            0,
            "BUTTON",
            "模型服务",
            owner_button,
            ID_NAV_MODEL,
            window,
            instance,
        )?;
        let nav_diagnostics = create_control(
            0,
            "BUTTON",
            "诊断工具",
            owner_button,
            ID_NAV_DIAGNOSTICS,
            window,
            instance,
        )?;

        let details_style = WS_CHILD
            | WS_VISIBLE
            | WS_VSCROLL
            | ES_MULTILINE as u32
            | ES_AUTOVSCROLL as u32
            | ES_READONLY as u32;
        let install_details = create_control(
            WS_EX_CLIENTEDGE,
            "EDIT",
            "正在检测...",
            details_style,
            0,
            window,
            instance,
        )?;
        let install = create_control(
            0,
            "BUTTON",
            "安装 / 更新",
            owner_button,
            ID_INSTALL,
            window,
            instance,
        )?;
        let uninstall = create_control(
            0,
            "BUTTON",
            "卸载",
            owner_button,
            ID_UNINSTALL,
            window,
            instance,
        )?;
        let install_refresh = create_control(
            0,
            "BUTTON",
            "刷新状态",
            owner_button,
            ID_INSTALL_REFRESH,
            window,
            instance,
        )?;

        let edit_style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32;
        let server_url =
            create_control(0, "EDIT", "", edit_style, ID_SERVER_URL, window, instance)?;
        let api_key = create_control(
            0,
            "EDIT",
            "",
            edit_style | ES_PASSWORD as u32,
            ID_API_KEY,
            window,
            instance,
        )?;
        let toggle_key = create_control(
            0,
            "BUTTON",
            "",
            owner_button,
            ID_TOGGLE_KEY,
            window,
            instance,
        )?;
        let test_connection = create_control(
            0,
            "BUTTON",
            "测试连接",
            owner_button,
            ID_TEST_CONNECTION,
            window,
            instance,
        )?;
        let image_model = create_control(
            0,
            "EDIT",
            model_config::IMAGE_MODEL,
            edit_style,
            ID_IMAGE_MODEL,
            window,
            instance,
        )?;
        let model_toggle = create_control(
            0,
            "BUTTON",
            "",
            owner_button,
            ID_MODEL_TOGGLE,
            window,
            instance,
        )?;
        let static_headers = create_control(
            0,
            "EDIT",
            "",
            edit_style | ES_PASSWORD as u32,
            ID_STATIC_HEADERS,
            window,
            instance,
        )?;
        let env_headers =
            create_control(0, "EDIT", "", edit_style, ID_ENV_HEADERS, window, instance)?;
        let restore_config = create_control(
            0,
            "BUTTON",
            "恢复配置",
            owner_button,
            ID_RESTORE_CONFIG,
            window,
            instance,
        )?;
        let save_config = create_control(
            0,
            "BUTTON",
            "保存并启用",
            owner_button,
            ID_SAVE_CONFIG,
            window,
            instance,
        )?;

        let diagnostics_details = create_control(
            WS_EX_CLIENTEDGE,
            "EDIT",
            "正在收集诊断信息...",
            details_style,
            0,
            window,
            instance,
        )?;
        let diagnostics_refresh = create_control(
            0,
            "BUTTON",
            "重新检测",
            owner_button,
            ID_DIAGNOSTICS_REFRESH,
            window,
            instance,
        )?;
        let diagnostics_export = create_control(
            0,
            "BUTTON",
            "导出诊断",
            owner_button,
            ID_DIAGNOSTICS_EXPORT,
            window,
            instance,
        )?;
        let launch_codex = create_control(
            0,
            "BUTTON",
            "启动 Codex",
            owner_button,
            ID_LAUNCH_CODEX,
            window,
            instance,
        )?;

        let fonts = create_fonts(dpi);
        let controls = Controls {
            nav_install,
            nav_model,
            nav_diagnostics,
            install_details,
            install,
            uninstall,
            install_refresh,
            server_url,
            api_key,
            toggle_key,
            test_connection,
            image_model,
            model_toggle,
            static_headers,
            env_headers,
            restore_config,
            save_config,
            diagnostics_details,
            diagnostics_refresh,
            diagnostics_export,
            launch_codex,
        };
        apply_control_fonts(&controls, &fonts);
        SendMessageW(api_key, EM_SETPASSWORDCHAR, 0x25cf, 0);
        SendMessageW(static_headers, EM_SETPASSWORDCHAR, 0x25cf, 0);
        apply_control_metrics(&controls, dpi);

        Ok(UiState {
            controls,
            fonts,
            input_brush: CreateSolidBrush(COLOR_INPUT),
            details_brush: CreateSolidBrush(COLOR_CARD),
            dpi,
            layout_dpi: dpi,
            page: Page::Model,
            page_controls_initialized: false,
            page_switch: PageSwitchQueue::default(),
            busy: false,
            busy_text: String::new(),
            key_visible: false,
            model_enabled: false,
            model_dirty: false,
            loading_model_controls: false,
            install_report: None,
            install_error: None,
            model_settings: None,
            model_error: None,
            connection_message: None,
        })
    }

    unsafe fn create_control(
        extended_style: u32,
        class_name: &str,
        text: &str,
        style: u32,
        id: i32,
        parent: HWND,
        instance: HINSTANCE,
    ) -> Result<HWND> {
        let control = CreateWindowExW(
            extended_style,
            wide(class_name).as_ptr(),
            wide(text).as_ptr(),
            style,
            0,
            0,
            0,
            0,
            parent,
            id as usize as _,
            instance,
            null_mut(),
        );
        if control.is_null() {
            bail!("failed to create {class_name} control");
        }
        Ok(control)
    }

    unsafe fn create_font(size: i32, weight: u32, face: &str) -> HFONT {
        CreateFontW(
            -size,
            0,
            0,
            0,
            weight as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            DEFAULT_PITCH as u32,
            wide(face).as_ptr(),
        )
    }

    unsafe fn create_fonts(dpi: u32) -> Fonts {
        Fonts {
            title: create_font(scale_for(dpi, 22), FW_BOLD, "Microsoft YaHei UI"),
            heading: create_font(scale_for(dpi, 15), FW_BOLD, "Microsoft YaHei UI"),
            body: create_font(scale_for(dpi, 13), FW_NORMAL, "Microsoft YaHei UI"),
            body_bold: create_font(scale_for(dpi, 13), FW_BOLD, "Microsoft YaHei UI"),
            small: create_font(scale_for(dpi, 12), FW_NORMAL, "Microsoft YaHei UI"),
            small_bold: create_font(scale_for(dpi, 12), FW_BOLD, "Microsoft YaHei UI"),
            mono: create_font(scale_for(dpi, 12), FW_NORMAL, "Consolas"),
        }
    }

    unsafe fn apply_control_fonts(controls: &Controls, fonts: &Fonts) {
        for control in [
            controls.nav_install,
            controls.nav_model,
            controls.nav_diagnostics,
            controls.install,
            controls.uninstall,
            controls.install_refresh,
            controls.server_url,
            controls.api_key,
            controls.toggle_key,
            controls.test_connection,
            controls.image_model,
            controls.model_toggle,
            controls.static_headers,
            controls.env_headers,
            controls.restore_config,
            controls.save_config,
            controls.diagnostics_refresh,
            controls.diagnostics_export,
            controls.launch_codex,
        ] {
            SendMessageW(control, WM_SETFONT, fonts.body as usize, 0);
        }
        SendMessageW(controls.install_details, WM_SETFONT, fonts.mono as usize, 0);
        SendMessageW(
            controls.diagnostics_details,
            WM_SETFONT,
            fonts.mono as usize,
            0,
        );
    }

    unsafe fn apply_control_metrics(controls: &Controls, dpi: u32) {
        SendMessageW(
            controls.server_url,
            EM_SETMARGINS,
            (EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize,
            margin_parameter(scale_for(dpi, 10), scale_for(dpi, 10)),
        );
        SendMessageW(
            controls.api_key,
            EM_SETMARGINS,
            (EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize,
            margin_parameter(scale_for(dpi, 10), scale_for(dpi, 42)),
        );
        for control in [
            controls.image_model,
            controls.static_headers,
            controls.env_headers,
        ] {
            SendMessageW(
                control,
                EM_SETMARGINS,
                (EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize,
                margin_parameter(scale_for(dpi, 10), scale_for(dpi, 10)),
            );
        }
    }

    unsafe fn update_dpi(window: HWND, dpi: u32, suggested: &RECT) {
        if let Some(state) = state_mut(window) {
            if state.dpi != dpi {
                let fonts = create_fonts(dpi);
                apply_control_fonts(&state.controls, &fonts);
                apply_control_metrics(&state.controls, dpi);
                let previous = std::mem::replace(&mut state.fonts, fonts);
                state.dpi = dpi;
                delete_fonts(&previous);
            }
        }
        SetWindowPos(
            window,
            null_mut(),
            suggested.left,
            suggested.top,
            suggested.right - suggested.left,
            suggested.bottom - suggested.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        layout(window);
        RedrawWindow(
            window,
            null(),
            null_mut(),
            RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW,
        );
    }

    unsafe fn load_app_icon(instance: HINSTANCE) -> *mut core::ffi::c_void {
        let icon = LoadIconW(instance, std::ptr::without_provenance::<u16>(1));
        if icon.is_null() {
            LoadIconW(null_mut(), IDI_APPLICATION)
        } else {
            icon
        }
    }

    unsafe fn delete_fonts(fonts: &Fonts) {
        for font in [
            fonts.title,
            fonts.heading,
            fonts.body,
            fonts.body_bold,
            fonts.small,
            fonts.small_bold,
            fonts.mono,
        ] {
            DeleteObject(font as _);
        }
    }

    unsafe fn handle_command(window: HWND, command: i32) {
        match command {
            ID_NAV_INSTALL => request_page(window, Page::Install),
            ID_NAV_MODEL => request_page(window, Page::Model),
            ID_NAV_DIAGNOSTICS => request_page(window, Page::Diagnostics),
            ID_INSTALL => start_operation(window, Operation::Install),
            ID_UNINSTALL => {
                let answer = MessageBoxW(
                    window,
                    wide("将移除图片兼容层，并恢复由本工具保存的模型配置。继续吗？").as_ptr(),
                    wide("确认卸载").as_ptr(),
                    MB_YESNO | MB_ICONWARNING,
                );
                if answer == IDYES {
                    start_operation(window, Operation::Uninstall);
                }
            }
            ID_INSTALL_REFRESH | ID_DIAGNOSTICS_REFRESH => {
                start_operation(window, Operation::Refresh)
            }
            ID_DIAGNOSTICS_EXPORT => start_operation(window, Operation::ExportDiagnostics),
            ID_LAUNCH_CODEX => start_operation(window, Operation::LaunchCodex),
            ID_TOGGLE_KEY => toggle_api_key_visibility(window),
            ID_TEST_CONNECTION => {
                if let Some(state) = state(window) {
                    match read_model_configuration(state) {
                        Ok(configuration) => {
                            start_operation(window, Operation::TestConnection(configuration))
                        }
                        Err(error) => show_error(window, &format_error(error)),
                    }
                }
            }
            ID_MODEL_TOGGLE => {
                if let Some(state) = state_mut(window) {
                    if !state.busy {
                        state.model_enabled = !state.model_enabled;
                        state.model_dirty = true;
                        InvalidateRect(window, null(), 0);
                    }
                }
            }
            ID_RESTORE_CONFIG => {
                let answer = MessageBoxW(
                    window,
                    wide("将恢复首次保存前的 config.toml 与 auth.json。继续吗？").as_ptr(),
                    wide("恢复模型配置").as_ptr(),
                    MB_YESNO | MB_ICONWARNING,
                );
                if answer == IDYES {
                    start_operation(window, Operation::RestoreModel);
                }
            }
            ID_SAVE_CONFIG => {
                if let Some(state) = state(window) {
                    let Some(settings) = state.model_settings.as_ref() else {
                        show_error(window, "模型配置尚未加载完成，请先刷新后再保存。");
                        return;
                    };
                    let configuration = match read_model_configuration(state) {
                        Ok(configuration) => configuration,
                        Err(error) => {
                            show_error(window, &format_error(error));
                            return;
                        }
                    };
                    let preview = match model_config::preview_settings(
                        &configuration.server_url,
                        &configuration.image_model,
                        configuration.image_generation_enabled,
                        &configuration.static_headers,
                        &configuration.env_headers,
                        settings,
                    ) {
                        Ok(preview) => preview,
                        Err(error) => {
                            show_error(window, &format_error(error));
                            return;
                        }
                    };
                    if MessageBoxW(
                        window,
                        wide(&preview).as_ptr(),
                        wide("确认配置变更").as_ptr(),
                        MB_YESNO | MB_ICONINFORMATION,
                    ) != IDYES
                    {
                        return;
                    }
                    start_operation(
                        window,
                        Operation::SaveModel {
                            configuration,
                            revisions: settings.revisions.clone(),
                        },
                    );
                }
            }
            _ => {}
        }
    }

    unsafe fn read_model_configuration(state: &UiState) -> Result<ModelConfiguration> {
        let static_headers = Zeroizing::new(read_text(state.controls.static_headers));
        let env_headers = read_text(state.controls.env_headers);
        Ok(ModelConfiguration {
            server_url: read_text(state.controls.server_url),
            api_key: read_text(state.controls.api_key),
            image_model: read_text(state.controls.image_model),
            image_generation_enabled: state.model_enabled,
            static_headers: model_config::parse_static_headers(&static_headers)?,
            env_headers: model_config::parse_env_headers(&env_headers)?,
        })
    }

    unsafe fn request_page(window: HWND, page: Page) {
        let Some(state) = state_mut(window) else {
            return;
        };
        if state.page_controls_initialized
            && state.page == page
            && !state.page_switch.message_pending
        {
            return;
        }
        if state.page_switch.request(page) && PostMessageW(window, WM_APPLY_PAGE, 0, 0) == 0 {
            let page = state.page_switch.take();
            if let Some(page) = page {
                apply_page(window, page);
            }
        }
    }

    unsafe fn apply_pending_page(window: HWND) {
        let page = state_mut(window).and_then(|state| state.page_switch.take());
        if let Some(page) = page {
            apply_page(window, page);
        }
    }

    unsafe fn apply_page(window: HWND, page: Page) {
        let Some(state) = state_mut(window) else {
            return;
        };
        if state.page_controls_initialized && state.page == page {
            return;
        }
        state.page = page;
        state.page_controls_initialized = true;
        let controls = &state.controls;
        let install_visible = page == Page::Install;
        let model_visible = page == Page::Model;
        let diagnostics_visible = page == Page::Diagnostics;
        let visibility = [
            (controls.install_details, install_visible),
            (controls.install, install_visible),
            (controls.uninstall, install_visible),
            (controls.install_refresh, install_visible),
            (controls.server_url, model_visible),
            (controls.api_key, model_visible),
            (controls.toggle_key, model_visible),
            (controls.test_connection, model_visible),
            (controls.image_model, model_visible),
            (controls.model_toggle, model_visible),
            (controls.static_headers, model_visible),
            (controls.env_headers, model_visible),
            (controls.restore_config, model_visible),
            (controls.save_config, model_visible),
            (controls.diagnostics_details, diagnostics_visible),
            (controls.diagnostics_refresh, diagnostics_visible),
            (controls.diagnostics_export, diagnostics_visible),
            (controls.launch_codex, diagnostics_visible),
        ];
        set_visibility_batch(&visibility);
        for navigation in [
            controls.nav_install,
            controls.nav_model,
            controls.nav_diagnostics,
        ] {
            InvalidateRect(navigation, null(), 0);
        }
        RedrawWindow(
            window,
            null(),
            null_mut(),
            RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW,
        );
    }

    unsafe fn set_visibility_batch(visibility: &[(HWND, bool)]) {
        let mut deferred = BeginDeferWindowPos(visibility.len() as i32);
        if !deferred.is_null() {
            for &(control, visible) in visibility {
                deferred = DeferWindowPos(
                    deferred,
                    control,
                    null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE
                        | SWP_NOSIZE
                        | SWP_NOZORDER
                        | SWP_NOACTIVATE
                        | SWP_NOREDRAW
                        | if visible {
                            SWP_SHOWWINDOW
                        } else {
                            SWP_HIDEWINDOW
                        },
                );
                if deferred.is_null() {
                    break;
                }
            }
            if !deferred.is_null() && EndDeferWindowPos(deferred) != 0 {
                return;
            }
        }
        for &(control, visible) in visibility {
            show(control, visible);
        }
    }

    unsafe fn show(control: HWND, visible: bool) {
        ShowWindow(control, if visible { SW_SHOW } else { SW_HIDE });
    }

    unsafe fn layout(window: HWND) {
        let Some(state) = state_mut(window) else {
            return;
        };
        let mut client: RECT = zeroed();
        GetClientRect(window, &mut client);
        let width_dpi = ((client.right.max(1) as i64 * 96) / 1004) as u32;
        let height_dpi = ((client.bottom.max(1) as i64 * 96) / 651) as u32;
        state.layout_dpi = state.dpi.min(width_dpi).min(height_dpi).max(72);
        let width = client.right;
        let height = client.bottom;
        let sidebar = scale(state, 218);
        let margin = scale(state, 34);
        let content_left = sidebar + margin;
        let content_right = width - margin;
        let action_y = height - scale(state, 72);

        MoveWindow(
            state.controls.nav_install,
            scale(state, 14),
            scale(state, 92),
            scale(state, 190),
            scale(state, 40),
            1,
        );
        MoveWindow(
            state.controls.nav_model,
            scale(state, 14),
            scale(state, 137),
            scale(state, 190),
            scale(state, 40),
            1,
        );
        MoveWindow(
            state.controls.nav_diagnostics,
            scale(state, 14),
            scale(state, 182),
            scale(state, 190),
            scale(state, 40),
            1,
        );

        let action_button_y = action_y + scale(state, 18);
        let action_button_height = scale(state, 36);
        MoveWindow(
            state.controls.install,
            content_right - scale(state, 116),
            action_button_y,
            scale(state, 116),
            action_button_height,
            1,
        );
        MoveWindow(
            state.controls.uninstall,
            content_right - scale(state, 212),
            action_button_y,
            scale(state, 86),
            action_button_height,
            1,
        );
        MoveWindow(
            state.controls.install_refresh,
            content_right - scale(state, 318),
            action_button_y,
            scale(state, 96),
            action_button_height,
            1,
        );
        MoveWindow(
            state.controls.install_details,
            content_left,
            scale(state, 264),
            (content_right - content_left).max(scale(state, 300)),
            (action_y - scale(state, 288)).max(scale(state, 150)),
            1,
        );

        let gap = scale(state, 16);
        let field_width = ((content_right - content_left - gap) / 2).max(scale(state, 210));
        let key_x = content_left + field_width + gap;
        let edit_y = scale(state, 174);
        let edit_height = scale(state, 40);
        MoveWindow(
            state.controls.server_url,
            content_left + scale(state, 10),
            edit_y + scale(state, 7),
            field_width - scale(state, 20),
            edit_height - scale(state, 12),
            1,
        );
        MoveWindow(
            state.controls.api_key,
            key_x + scale(state, 10),
            edit_y + scale(state, 7),
            field_width - scale(state, 54),
            edit_height - scale(state, 12),
            1,
        );
        MoveWindow(
            state.controls.toggle_key,
            key_x + field_width - scale(state, 37),
            edit_y + scale(state, 4),
            scale(state, 32),
            scale(state, 32),
            1,
        );
        MoveWindow(
            state.controls.test_connection,
            content_right - scale(state, 94),
            scale(state, 222),
            scale(state, 94),
            action_button_height,
            1,
        );
        MoveWindow(
            state.controls.image_model,
            content_left + scale(state, 48),
            scale(state, 338),
            (content_right - content_left - scale(state, 118)).max(scale(state, 260)),
            scale(state, 34),
            1,
        );
        MoveWindow(
            state.controls.model_toggle,
            content_right - scale(state, 44),
            scale(state, 346),
            scale(state, 44),
            scale(state, 24),
            1,
        );
        let header_width = ((content_right - content_left - gap) / 2).max(scale(state, 210));
        MoveWindow(
            state.controls.static_headers,
            content_left + scale(state, 10),
            scale(state, 470),
            header_width - scale(state, 20),
            scale(state, 28),
            1,
        );
        MoveWindow(
            state.controls.env_headers,
            content_left + header_width + gap + scale(state, 10),
            scale(state, 470),
            header_width - scale(state, 20),
            scale(state, 28),
            1,
        );
        MoveWindow(
            state.controls.restore_config,
            content_right - scale(state, 222),
            action_button_y,
            scale(state, 94),
            action_button_height,
            1,
        );
        MoveWindow(
            state.controls.save_config,
            content_right - scale(state, 116),
            action_button_y,
            scale(state, 116),
            action_button_height,
            1,
        );

        MoveWindow(
            state.controls.diagnostics_details,
            content_left,
            scale(state, 142),
            (content_right - content_left).max(scale(state, 300)),
            (action_y - scale(state, 166)).max(scale(state, 250)),
            1,
        );
        MoveWindow(
            state.controls.diagnostics_refresh,
            content_right - scale(state, 100),
            action_button_y,
            scale(state, 100),
            action_button_height,
            1,
        );
        MoveWindow(
            state.controls.diagnostics_export,
            content_right - scale(state, 212),
            action_button_y,
            scale(state, 102),
            action_button_height,
            1,
        );
        MoveWindow(
            state.controls.launch_codex,
            content_right - scale(state, 330),
            action_button_y,
            scale(state, 108),
            action_button_height,
            1,
        );
    }

    unsafe fn paint_window(window: HWND) {
        let Some(state) = state(window) else {
            return;
        };
        let mut paint: PAINTSTRUCT = zeroed();
        let hdc = BeginPaint(window, &mut paint);
        let mut client: RECT = zeroed();
        GetClientRect(window, &mut client);
        let width = client.right.max(1);
        let height = client.bottom.max(1);
        let memory_dc = CreateCompatibleDC(hdc);
        if memory_dc.is_null() {
            paint_window_content(hdc, state, client);
            EndPaint(window, &paint);
            return;
        }
        let bitmap = CreateCompatibleBitmap(hdc, width, height);
        if bitmap.is_null() {
            DeleteDC(memory_dc);
            paint_window_content(hdc, state, client);
            EndPaint(window, &paint);
            return;
        }
        let previous_bitmap = SelectObject(memory_dc, bitmap as _);
        paint_window_content(memory_dc, state, client);
        BitBlt(hdc, 0, 0, width, height, memory_dc, 0, 0, SRCCOPY);
        SelectObject(memory_dc, previous_bitmap);
        DeleteObject(bitmap as _);
        DeleteDC(memory_dc);
        EndPaint(window, &paint);
    }

    unsafe fn paint_window_content(hdc: HDC, state: &UiState, client: RECT) {
        paint_background(hdc, state, client);
        paint_sidebar(hdc, state, client);
        paint_header(hdc, state, client);
        match state.page {
            Page::Install => paint_install_page(hdc, state, client),
            Page::Model => paint_model_page(hdc, state, client),
            Page::Diagnostics => paint_diagnostics_page(hdc, state, client),
        }
    }

    unsafe fn paint_background(hdc: HDC, state: &UiState, client: RECT) {
        fill(hdc, client, COLOR_WHITE);
        fill(
            hdc,
            RECT {
                right: scale(state, 218),
                ..client
            },
            COLOR_SIDEBAR,
        );
        fill(
            hdc,
            RECT {
                left: scale(state, 218),
                top: client.bottom - scale(state, 72),
                ..client
            },
            COLOR_ACTIONBAR,
        );
        fill(
            hdc,
            RECT {
                left: scale(state, 218) - 1,
                right: scale(state, 218),
                ..client
            },
            COLOR_BORDER,
        );
        fill(
            hdc,
            RECT {
                left: scale(state, 218),
                top: scale(state, 91),
                right: client.right,
                bottom: scale(state, 92),
            },
            COLOR_BORDER,
        );
        fill(
            hdc,
            RECT {
                left: scale(state, 218),
                top: client.bottom - scale(state, 72),
                right: client.right,
                bottom: client.bottom - scale(state, 71),
            },
            COLOR_BORDER,
        );
    }

    unsafe fn paint_sidebar(hdc: HDC, state: &UiState, client: RECT) {
        draw_text(
            hdc,
            "图片显示修复工具",
            logical_rect(state, 24, 22, 176, 26),
            state.fonts.heading,
            COLOR_TEXT,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        draw_text(
            hdc,
            "Designed by",
            logical_rect(state, 24, 50, 74, 20),
            state.fonts.small,
            COLOR_MUTED,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        draw_text(
            hdc,
            "comidea.org",
            logical_rect(state, 94, 50, 100, 20),
            state.fonts.small_bold,
            COLOR_TEAL,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );

        let footer_y = client.bottom - scale(state, 58);
        fill(
            hdc,
            RECT {
                left: scale(state, 14),
                top: footer_y - scale(state, 12),
                right: scale(state, 204),
                bottom: footer_y - scale(state, 11),
            },
            COLOR_BORDER,
        );
        draw_text(
            hdc,
            &format!("Codex Image Bridge {}", env!("CARGO_PKG_VERSION")),
            RECT {
                left: scale(state, 24),
                top: footer_y,
                right: scale(state, 204),
                bottom: footer_y + scale(state, 20),
            },
            state.fonts.small_bold,
            COLOR_BODY,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        draw_text(
            hdc,
            "Windows x64 · 单文件部署",
            RECT {
                left: scale(state, 24),
                top: footer_y + scale(state, 20),
                right: scale(state, 204),
                bottom: footer_y + scale(state, 40),
            },
            state.fonts.small,
            COLOR_MUTED,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }

    unsafe fn paint_header(hdc: HDC, state: &UiState, client: RECT) {
        let (title, subtitle) = match state.page {
            Page::Install => ("安装状态", "安装、更新或移除 Codex 图片兼容层"),
            Page::Model => ("模型服务配置", "自定义图片生成服务与 gpt-image-2"),
            Page::Diagnostics => ("诊断工具", "查看 Codex 路径、集成状态与配置位置"),
        };
        draw_text(
            hdc,
            title,
            logical_rect(state, 252, 18, 420, 30),
            state.fonts.title,
            COLOR_TEXT,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        draw_text(
            hdc,
            subtitle,
            logical_rect(state, 252, 51, 520, 22),
            state.fonts.body,
            COLOR_MUTED,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        let (health, color) = header_health(state);
        let right = client.right - scale(state, 34);
        draw_dot(hdc, right - scale(state, 104), scale(state, 41), color);
        draw_text(
            hdc,
            health,
            RECT {
                left: right - scale(state, 92),
                top: scale(state, 27),
                right,
                bottom: scale(state, 57),
            },
            state.fonts.body_bold,
            color,
            DT_RIGHT_FLAGS,
        );
    }

    unsafe fn paint_install_page(hdc: HDC, state: &UiState, client: RECT) {
        let left = scale(state, 252);
        let right = client.right - scale(state, 34);
        section_heading(
            hdc,
            state,
            "Codex 图片兼容层",
            install_status(state),
            scale(state, 116),
            install_status_color(state),
            right,
        );
        let gap = scale(state, 12);
        let card_width = (right - left - gap * 2) / 3;
        let report = state.install_report.as_ref();
        status_card(
            hdc,
            state,
            RECT {
                left,
                top: scale(state, 154),
                right: left + card_width,
                bottom: scale(state, 210),
            },
            "环境入口",
            report.is_some_and(|report| report.environment_healthy),
            report.is_some(),
        );
        status_card(
            hdc,
            state,
            RECT {
                left: left + card_width + gap,
                top: scale(state, 154),
                right: left + card_width * 2 + gap,
                bottom: scale(state, 210),
            },
            "图片代理",
            report.is_some_and(|report| report.proxy_healthy),
            report.is_some(),
        );
        status_card(
            hdc,
            state,
            RECT {
                left: left + (card_width + gap) * 2,
                top: scale(state, 154),
                right,
                bottom: scale(state, 210),
            },
            "命令别名",
            report.is_some_and(|report| report.alias_healthy),
            report.is_some(),
        );
        section_heading(
            hdc,
            state,
            "安装详情",
            "",
            scale(state, 226),
            COLOR_MUTED,
            right,
        );
        paint_restart_note(
            hdc,
            state,
            client,
            "安装或卸载完成后，请重新启动 Codex Desktop",
        );
    }

    unsafe fn paint_model_page(hdc: HDC, state: &UiState, client: RECT) {
        let left = scale(state, 252);
        let right = client.right - scale(state, 34);
        let server_status = if state.model_dirty {
            "有未保存修改"
        } else if state
            .model_settings
            .as_ref()
            .is_some_and(|settings| settings.managed)
        {
            "配置已保存"
        } else if state
            .model_settings
            .as_ref()
            .is_some_and(|settings| !settings.server_url.is_empty())
        {
            "已读取现有配置"
        } else {
            "尚未配置"
        };
        section_heading(
            hdc,
            state,
            "自定义服务器",
            server_status,
            scale(state, 116),
            if server_status == "尚未配置" {
                COLOR_MUTED
            } else if state.model_dirty {
                COLOR_AMBER
            } else {
                COLOR_GREEN
            },
            right,
        );
        draw_text(
            hdc,
            "服务器地址",
            logical_rect(state, 252, 148, 180, 22),
            state.fonts.small_bold,
            COLOR_BODY,
            TEXT_LEFT,
        );
        let gap = scale(state, 16);
        let field_width = (right - left - gap) / 2;
        let focused = GetFocus();
        rounded_rect(
            hdc,
            RECT {
                left,
                top: scale(state, 174),
                right: left + field_width,
                bottom: scale(state, 214),
            },
            COLOR_INPUT,
            if focused == state.controls.server_url {
                COLOR_TEAL
            } else {
                COLOR_BORDER_DARK
            },
            scale(state, 6),
        );
        rounded_rect(
            hdc,
            RECT {
                left: left + field_width + gap,
                top: scale(state, 174),
                right,
                bottom: scale(state, 214),
            },
            COLOR_INPUT,
            if focused == state.controls.api_key {
                COLOR_TEAL
            } else {
                COLOR_BORDER_DARK
            },
            scale(state, 6),
        );
        draw_text(
            hdc,
            "API Key",
            RECT {
                left: left + field_width + gap,
                top: scale(state, 148),
                right,
                bottom: scale(state, 170),
            },
            state.fonts.small_bold,
            COLOR_BODY,
            TEXT_LEFT,
        );
        let (connection, tone) = state.connection_message.as_ref().map_or(
            ("尚未测试连接", MessageTone::Muted),
            |(message, tone)| (message.as_str(), *tone),
        );
        let connection_color = tone_color(tone);
        draw_check(
            hdc,
            state,
            left + scale(state, 8),
            scale(state, 237),
            matches!(tone, MessageTone::Good),
            matches!(tone, MessageTone::Good | MessageTone::Error),
        );
        draw_text(
            hdc,
            connection,
            RECT {
                left: left + scale(state, 24),
                top: scale(state, 224),
                right: right - scale(state, 110),
                bottom: scale(state, 250),
            },
            state.fonts.small,
            connection_color,
            TEXT_LEFT,
        );
        divider(hdc, state, left, right, 276);

        section_heading(
            hdc,
            state,
            "图片模型",
            if state.model_enabled {
                "已启用"
            } else {
                "未启用"
            },
            scale(state, 300),
            if state.model_enabled {
                COLOR_GREEN
            } else {
                COLOR_MUTED
            },
            right,
        );
        rounded_rect(
            hdc,
            logical_rect(state, 252, 339, 28, 28),
            rgb(237, 240, 244),
            rgb(237, 240, 244),
            6,
        );
        draw_text(
            hdc,
            "AI",
            logical_rect(state, 252, 339, 28, 28),
            state.fonts.small_bold,
            COLOR_BODY,
            TEXT_CENTER,
        );
        rounded_rect(
            hdc,
            RECT {
                left: left + scale(state, 38),
                top: scale(state, 334),
                right: right - scale(state, 62),
                bottom: scale(state, 376),
            },
            COLOR_INPUT,
            if focused == state.controls.image_model {
                COLOR_TEAL
            } else {
                COLOR_BORDER_DARK
            },
            scale(state, 6),
        );
        let provider = state
            .model_settings
            .as_ref()
            .map(|settings| settings.provider_id.as_str())
            .unwrap_or("-");
        draw_text(
            hdc,
            &format!(
                "Provider: {provider} · 图片生成{} · 不修改当前文本模型",
                if state.model_enabled {
                    "已打开"
                } else {
                    "未打开"
                }
            ),
            logical_rect(state, 290, 380, 500, 22),
            state.fonts.small,
            COLOR_MUTED,
            TEXT_LEFT,
        );
        divider(hdc, state, left, right, 411);
        section_heading(
            hdc,
            state,
            "高级 Header",
            "JSON 格式，敏感内容默认隐藏",
            scale(state, 429),
            COLOR_MUTED,
            right,
        );
        let header_width = (right - left - scale(state, 16)) / 2;
        draw_text(
            hdc,
            "静态 Header",
            RECT {
                left,
                top: scale(state, 451),
                right: left + header_width,
                bottom: scale(state, 471),
            },
            state.fonts.small_bold,
            COLOR_BODY,
            TEXT_LEFT,
        );
        draw_text(
            hdc,
            "环境 Header（值为环境变量名）",
            RECT {
                left: left + header_width + scale(state, 16),
                top: scale(state, 451),
                right,
                bottom: scale(state, 471),
            },
            state.fonts.small_bold,
            COLOR_BODY,
            TEXT_LEFT,
        );
        rounded_rect(
            hdc,
            RECT {
                left,
                top: scale(state, 466),
                right: left + header_width,
                bottom: scale(state, 506),
            },
            COLOR_INPUT,
            if focused == state.controls.static_headers {
                COLOR_TEAL
            } else {
                COLOR_BORDER_DARK
            },
            scale(state, 6),
        );
        rounded_rect(
            hdc,
            RECT {
                left: left + header_width + scale(state, 16),
                top: scale(state, 466),
                right,
                bottom: scale(state, 506),
            },
            COLOR_INPUT,
            if focused == state.controls.env_headers {
                COLOR_TEAL
            } else {
                COLOR_BORDER_DARK
            },
            scale(state, 6),
        );
        paint_restart_note(hdc, state, client, "配置变更后需要重新启动 Codex Desktop");
    }

    unsafe fn paint_diagnostics_page(hdc: HDC, state: &UiState, client: RECT) {
        section_heading(
            hdc,
            state,
            "当前环境",
            if state.install_error.is_none() && state.model_error.is_none() {
                "检测完成"
            } else {
                "存在异常"
            },
            scale(state, 116),
            if state.install_error.is_none() && state.model_error.is_none() {
                COLOR_GREEN
            } else {
                COLOR_RED
            },
            client.right - scale(state, 34),
        );
        paint_restart_note(hdc, state, client, "刷新可重新检测安装状态与配置位置");
    }

    unsafe fn paint_restart_note(hdc: HDC, state: &UiState, client: RECT, text: &str) {
        let y = client.bottom - scale(state, 54);
        draw_text(
            hdc,
            "↻",
            RECT {
                left: scale(state, 252),
                top: y,
                right: scale(state, 272),
                bottom: y + scale(state, 36),
            },
            state.fonts.heading,
            COLOR_AMBER,
            TEXT_CENTER,
        );
        draw_text(
            hdc,
            text,
            RECT {
                left: scale(state, 276),
                top: y,
                right: client.right - scale(state, 340),
                bottom: y + scale(state, 36),
            },
            state.fonts.small,
            COLOR_AMBER,
            TEXT_LEFT,
        );
    }

    unsafe fn section_heading(
        hdc: HDC,
        state: &UiState,
        title: &str,
        status: &str,
        y: i32,
        status_color: COLORREF,
        right: i32,
    ) {
        draw_text(
            hdc,
            title,
            RECT {
                left: scale(state, 252),
                top: y,
                right: right - scale(state, 160),
                bottom: y + scale(state, 28),
            },
            state.fonts.heading,
            COLOR_TEXT,
            TEXT_LEFT,
        );
        if !status.is_empty() {
            draw_text(
                hdc,
                status,
                RECT {
                    left: right - scale(state, 160),
                    top: y,
                    right,
                    bottom: y + scale(state, 28),
                },
                state.fonts.small,
                status_color,
                DT_RIGHT_FLAGS,
            );
        }
    }

    unsafe fn status_card(
        hdc: HDC,
        state: &UiState,
        rect: RECT,
        title: &str,
        healthy: bool,
        known: bool,
    ) {
        rounded_rect(hdc, rect, COLOR_CARD, COLOR_BORDER, 6);
        draw_check(
            hdc,
            state,
            rect.left + scale(state, 18),
            (rect.top + rect.bottom) / 2,
            healthy,
            known,
        );
        draw_text(
            hdc,
            title,
            RECT {
                left: rect.left + scale(state, 40),
                top: rect.top + scale(state, 7),
                right: rect.right - scale(state, 8),
                bottom: rect.top + scale(state, 29),
            },
            state.fonts.small_bold,
            COLOR_TEXT,
            TEXT_LEFT,
        );
        let (status, color) = if !known {
            ("检测中", COLOR_MUTED)
        } else if healthy {
            ("正常", COLOR_GREEN)
        } else {
            ("异常", COLOR_RED)
        };
        draw_text(
            hdc,
            status,
            RECT {
                left: rect.left + scale(state, 40),
                top: rect.top + scale(state, 27),
                right: rect.right - scale(state, 8),
                bottom: rect.bottom - scale(state, 5),
            },
            state.fonts.small,
            color,
            TEXT_LEFT,
        );
    }

    unsafe fn divider(hdc: HDC, state: &UiState, left: i32, right: i32, y: i32) {
        fill(
            hdc,
            RECT {
                left,
                top: scale(state, y),
                right,
                bottom: scale(state, y) + 1,
            },
            COLOR_BORDER,
        );
    }

    unsafe fn draw_button(window: HWND, item: &DRAWITEMSTRUCT) {
        let Some(state) = state(window) else {
            return;
        };
        let id = item.CtlID as i32;
        let disabled = item.itemState & ODS_DISABLED != 0;
        let pressed = item.itemState & ODS_SELECTED != 0;
        if matches!(id, ID_NAV_INSTALL | ID_NAV_MODEL | ID_NAV_DIAGNOSTICS) {
            draw_nav_button(state, item, id, disabled, pressed);
            return;
        }
        if id == ID_MODEL_TOGGLE {
            draw_toggle(state, item, disabled);
            return;
        }
        if id == ID_TOGGLE_KEY {
            draw_eye_button(state, item, disabled);
            return;
        }

        let primary = matches!(id, ID_INSTALL | ID_SAVE_CONFIG);
        let danger = id == ID_UNINSTALL;
        let mut fill_color = if primary { COLOR_TEAL } else { COLOR_WHITE };
        if pressed {
            fill_color = if primary {
                COLOR_TEAL_DARK
            } else {
                rgb(239, 242, 245)
            };
        }
        let border = if primary {
            COLOR_TEAL
        } else {
            COLOR_BORDER_DARK
        };
        let color = if disabled {
            rgb(150, 157, 165)
        } else if primary {
            COLOR_WHITE
        } else if danger {
            COLOR_RED
        } else {
            COLOR_TEXT
        };
        rounded_rect(item.hDC, item.rcItem, fill_color, border, 6);
        let label = button_label(id);
        draw_text(
            item.hDC,
            label,
            item.rcItem,
            state.fonts.body_bold,
            color,
            TEXT_CENTER,
        );
        if item.itemState & ODS_FOCUS != 0 {
            let focus = inset_rect(item.rcItem, scale(state, 4));
            DrawFocusRect(item.hDC, &focus);
        }
    }

    unsafe fn draw_nav_button(
        state: &UiState,
        item: &DRAWITEMSTRUCT,
        id: i32,
        disabled: bool,
        pressed: bool,
    ) {
        let selected = matches!(
            (state.page, id),
            (Page::Install, ID_NAV_INSTALL)
                | (Page::Model, ID_NAV_MODEL)
                | (Page::Diagnostics, ID_NAV_DIAGNOSTICS)
        );
        let fill_color = if selected {
            if pressed {
                COLOR_TEAL_DARK
            } else {
                COLOR_TEAL
            }
        } else if pressed {
            rgb(231, 235, 239)
        } else {
            COLOR_SIDEBAR
        };
        rounded_rect(item.hDC, item.rcItem, fill_color, fill_color, 6);
        let color = if disabled {
            COLOR_MUTED
        } else if selected {
            COLOR_WHITE
        } else {
            rgb(75, 86, 99)
        };
        let (icon, label) = match id {
            ID_NAV_INSTALL => ("□", "安装状态"),
            ID_NAV_MODEL => ("⚙", "模型服务"),
            _ => ("◇", "诊断工具"),
        };
        let icon_rect = RECT {
            left: item.rcItem.left + scale(state, 10),
            top: item.rcItem.top,
            right: item.rcItem.left + scale(state, 36),
            bottom: item.rcItem.bottom,
        };
        draw_text(
            item.hDC,
            icon,
            icon_rect,
            state.fonts.body_bold,
            color,
            TEXT_CENTER,
        );
        let label_rect = RECT {
            left: item.rcItem.left + scale(state, 42),
            top: item.rcItem.top,
            right: item.rcItem.right - scale(state, 8),
            bottom: item.rcItem.bottom,
        };
        draw_text(
            item.hDC,
            label,
            label_rect,
            if selected {
                state.fonts.body_bold
            } else {
                state.fonts.body
            },
            color,
            TEXT_LEFT,
        );
    }

    unsafe fn draw_toggle(state: &UiState, item: &DRAWITEMSTRUCT, disabled: bool) {
        let enabled = state.model_enabled;
        let track = if disabled {
            rgb(190, 196, 201)
        } else if enabled {
            rgb(34, 129, 91)
        } else {
            rgb(177, 185, 193)
        };
        rounded_rect(item.hDC, item.rcItem, track, track, 12);
        let size = scale(state, 18);
        let margin = scale(state, 3);
        let left = if enabled {
            item.rcItem.right - margin - size
        } else {
            item.rcItem.left + margin
        };
        rounded_rect(
            item.hDC,
            RECT {
                left,
                top: item.rcItem.top + margin,
                right: left + size,
                bottom: item.rcItem.top + margin + size,
            },
            COLOR_WHITE,
            COLOR_WHITE,
            9,
        );
    }

    unsafe fn draw_eye_button(state: &UiState, item: &DRAWITEMSTRUCT, disabled: bool) {
        fill(item.hDC, item.rcItem, COLOR_INPUT);
        draw_text(
            item.hDC,
            if state.key_visible { "◉" } else { "◎" },
            item.rcItem,
            state.fonts.heading,
            if disabled {
                rgb(160, 166, 173)
            } else {
                COLOR_MUTED
            },
            TEXT_CENTER,
        );
    }

    unsafe fn start_operation(window: HWND, operation: Operation) {
        let Some(state) = state_mut(window) else {
            return;
        };
        if state.busy {
            return;
        }
        state.busy = true;
        state.busy_text = match &operation {
            Operation::Refresh => "正在检测",
            Operation::Install => "正在安装并验证",
            Operation::Uninstall => "正在卸载并恢复",
            Operation::TestConnection(_) => "正在测试连接",
            Operation::SaveModel { .. } => "正在保存配置",
            Operation::RestoreModel => "正在恢复配置",
            Operation::ExportDiagnostics => "正在导出诊断",
            Operation::LaunchCodex => "正在启动 Codex",
        }
        .to_owned();
        if matches!(operation, Operation::TestConnection(_)) {
            state.connection_message = Some(("正在连接服务器...".to_owned(), MessageTone::Muted));
        }
        sync_enabled_state(state);
        InvalidateRect(window, null(), 0);

        let window_value = window as isize;
        thread::spawn(move || {
            let result = match operation {
                Operation::Refresh => OperationResult::Refresh {
                    report: install::status_report().map_err(format_error),
                    settings: model_config::load_settings().map_err(format_error),
                },
                Operation::Install => {
                    let action = install::install(None).map_err(format_error);
                    let report = install::status_report().map_err(format_error);
                    OperationResult::Install { action, report }
                }
                Operation::Uninstall => {
                    let action = install::uninstall().map_err(format_error);
                    let report = install::status_report().map_err(format_error);
                    let settings = model_config::load_settings().map_err(format_error);
                    OperationResult::Uninstall {
                        action,
                        report,
                        settings,
                    }
                }
                Operation::TestConnection(configuration) => {
                    let result =
                        model_config::test_connection(&configuration).map_err(format_error);
                    OperationResult::TestConnection(result)
                }
                Operation::SaveModel {
                    configuration,
                    revisions,
                } => {
                    let action = model_config::save_settings(&configuration, &revisions)
                        .map(|_| ())
                        .map_err(format_error);
                    let settings = action
                        .is_ok()
                        .then(|| model_config::load_settings().map_err(format_error));
                    OperationResult::SaveModel { action, settings }
                }
                Operation::RestoreModel => {
                    let action = model_config::restore_managed_config().map_err(format_error);
                    let settings = action
                        .as_ref()
                        .is_ok_and(|restored| *restored)
                        .then(|| model_config::load_settings().map_err(format_error));
                    OperationResult::RestoreModel { action, settings }
                }
                Operation::ExportDiagnostics => OperationResult::ExportDiagnostics(
                    diagnostics::create_support_bundle(None)
                        .and_then(|path| {
                            diagnostics::require_bundle_is_redacted(&path)?;
                            Ok(path)
                        })
                        .map_err(format_error),
                ),
                Operation::LaunchCodex => {
                    OperationResult::LaunchCodex(diagnostics::launch_codex().map_err(format_error))
                }
            };
            let pointer = Box::into_raw(Box::new(result));
            if unsafe {
                PostMessageW(
                    window_value as HWND,
                    WM_OPERATION_COMPLETE,
                    0,
                    pointer as isize,
                )
            } == 0
            {
                unsafe { drop(Box::from_raw(pointer)) };
            }
        });
    }

    unsafe fn complete_operation(window: HWND, result: OperationResult) {
        let Some(state) = state_mut(window) else {
            return;
        };
        state.busy = false;
        state.busy_text.clear();
        match result {
            OperationResult::Refresh { report, settings } => {
                apply_report(state, report);
                apply_settings(state, settings, true);
            }
            OperationResult::Install { action, report } => {
                apply_report(state, report);
                show_action_result(
                    window,
                    action,
                    "安装完成。现在可以关闭本窗口；请完全退出并重新打开 Codex Desktop。",
                );
            }
            OperationResult::Uninstall {
                action,
                report,
                settings,
            } => {
                apply_report(state, report);
                match action {
                    Ok(outcome) => {
                        apply_settings(state, settings, true);
                        if let Some(warning) = outcome.model_config_warning {
                            let message = format!(
                                "图片显示代理已卸载。检测到模型配置在保存后被修改，为避免覆盖新内容，配置及恢复备份已保留。\r\n\r\n详细信息：{warning}\r\n\r\n请重新启动 Codex Desktop。"
                            );
                            MessageBoxW(
                                window,
                                wide(&message).as_ptr(),
                                wide("卸载部分完成").as_ptr(),
                                MB_OK | MB_ICONWARNING,
                            );
                        } else if outcome.files_pending_cleanup {
                            MessageBoxW(
                                window,
                                wide("图片兼容集成已卸载。Codex 正在占用部分程序文件，完全退出 Codex 后即可删除残留安装目录；这不影响卸载结果。")
                                    .as_ptr(),
                                wide("卸载完成").as_ptr(),
                                MB_OK | MB_ICONINFORMATION,
                            );
                        } else {
                            MessageBoxW(
                                window,
                                wide("卸载完成。已恢复本工具管理且未被后续修改的配置；请重新启动 Codex Desktop。")
                                    .as_ptr(),
                                wide("操作完成").as_ptr(),
                                MB_OK | MB_ICONINFORMATION,
                            );
                        }
                    }
                    Err(error) => show_error(window, &error),
                }
            }
            OperationResult::TestConnection(result) => match result {
                Ok(report) => {
                    let tone = if report.usable {
                        MessageTone::Good
                    } else {
                        MessageTone::Error
                    };
                    state.connection_message = Some((report.summary, tone));
                }
                Err(error) => {
                    state.connection_message =
                        Some((format!("连接失败 · {error}"), MessageTone::Error));
                }
            },
            OperationResult::SaveModel { action, settings } => match action {
                Ok(()) => {
                    if let Some(settings) = settings {
                        apply_settings(state, settings, true);
                    }
                    MessageBoxW(
                        window,
                        wide("配置已保存。请重新启动 Codex Desktop 使模型列表和图片生成功能生效。")
                            .as_ptr(),
                        wide("保存完成").as_ptr(),
                        MB_OK | MB_ICONINFORMATION,
                    );
                }
                Err(error) => show_error(window, &error),
            },
            OperationResult::RestoreModel { action, settings } => match action {
                Ok(true) => {
                    if let Some(settings) = settings {
                        apply_settings(state, settings, true);
                    }
                    MessageBoxW(
                        window,
                        wide("已恢复首次保存前的模型配置。").as_ptr(),
                        wide("恢复完成").as_ptr(),
                        MB_OK | MB_ICONINFORMATION,
                    );
                }
                Ok(false) => {
                    MessageBoxW(
                        window,
                        wide("没有找到由本工具管理的模型配置备份。").as_ptr(),
                        wide("无需恢复").as_ptr(),
                        MB_OK | MB_ICONINFORMATION,
                    );
                }
                Err(error) => show_error(window, &error),
            },
            OperationResult::ExportDiagnostics(result) => match result {
                Ok(path) => {
                    let message = format!(
                        "脱敏诊断包已生成：\r\n\r\n{}\r\n\r\n其中不包含 API Key、Header 值、Base64、提示词或会话正文。",
                        path.display()
                    );
                    MessageBoxW(
                        window,
                        wide(&message).as_ptr(),
                        wide("导出完成").as_ptr(),
                        MB_OK | MB_ICONINFORMATION,
                    );
                }
                Err(error) => show_error(window, &error),
            },
            OperationResult::LaunchCodex(result) => {
                show_action_result(window, result, "已提交 Codex 启动请求。")
            }
        }
        refresh_text_views(state);
        sync_enabled_state(state);
        InvalidateRect(window, null(), 0);
    }

    unsafe fn apply_report(state: &mut UiState, report: std::result::Result<StatusReport, String>) {
        match report {
            Ok(report) => {
                state.install_report = Some(report);
                state.install_error = None;
            }
            Err(error) => {
                state.install_report = None;
                state.install_error = Some(error);
            }
        }
    }

    unsafe fn apply_settings(
        state: &mut UiState,
        settings: std::result::Result<ModelSettings, String>,
        update_controls: bool,
    ) {
        match settings {
            Ok(settings) => {
                state.model_enabled = settings.image_model_enabled;
                if update_controls {
                    state.loading_model_controls = true;
                    let static_headers = Zeroizing::new(
                        model_config::format_headers_json(&settings.static_headers)
                            .unwrap_or_default(),
                    );
                    let env_headers = model_config::format_headers_json(&settings.env_headers)
                        .unwrap_or_default();
                    set_text(state.controls.server_url, &settings.server_url);
                    set_text(state.controls.api_key, &settings.api_key);
                    set_text(state.controls.image_model, &settings.image_model);
                    set_text(state.controls.static_headers, &static_headers);
                    set_text(state.controls.env_headers, &env_headers);
                    state.loading_model_controls = false;
                    state.model_dirty = false;
                }
                state.model_settings = Some(settings);
                state.model_error = None;
            }
            Err(error) => {
                state.model_settings = None;
                state.model_error = Some(error);
            }
        }
    }

    unsafe fn refresh_text_views(state: &UiState) {
        set_text(
            state.controls.install_details,
            &format_install_details(state),
        );
        set_text(
            state.controls.diagnostics_details,
            &format_diagnostics(state),
        );
    }

    unsafe fn sync_enabled_state(state: &UiState) {
        let enabled = !state.busy;
        for control in [
            state.controls.install,
            state.controls.install_refresh,
            state.controls.server_url,
            state.controls.api_key,
            state.controls.toggle_key,
            state.controls.test_connection,
            state.controls.image_model,
            state.controls.model_toggle,
            state.controls.static_headers,
            state.controls.env_headers,
            state.controls.save_config,
            state.controls.diagnostics_refresh,
            state.controls.diagnostics_export,
            state.controls.launch_codex,
        ] {
            EnableWindow(control, enabled as i32);
        }
        EnableWindow(
            state.controls.uninstall,
            (enabled
                && state
                    .install_report
                    .as_ref()
                    .is_some_and(|report| report.state_present)) as i32,
        );
        EnableWindow(
            state.controls.restore_config,
            (enabled
                && state
                    .model_settings
                    .as_ref()
                    .is_some_and(|settings| settings.managed)) as i32,
        );
    }

    unsafe fn toggle_api_key_visibility(window: HWND) {
        let Some(state) = state_mut(window) else {
            return;
        };
        if state.busy {
            return;
        }
        state.key_visible = !state.key_visible;
        SendMessageW(
            state.controls.api_key,
            EM_SETPASSWORDCHAR,
            if state.key_visible { 0 } else { 0x25cf },
            0,
        );
        SendMessageW(
            state.controls.static_headers,
            EM_SETPASSWORDCHAR,
            if state.key_visible { 0 } else { 0x25cf },
            0,
        );
        InvalidateRect(state.controls.api_key, null(), 1);
        InvalidateRect(state.controls.static_headers, null(), 1);
        InvalidateRect(state.controls.toggle_key, null(), 1);
        SetFocus(state.controls.api_key);
    }

    unsafe fn show_action_result(
        window: HWND,
        result: std::result::Result<(), String>,
        success: &str,
    ) {
        match result {
            Ok(()) => {
                MessageBoxW(
                    window,
                    wide(success).as_ptr(),
                    wide("操作完成").as_ptr(),
                    MB_OK | MB_ICONINFORMATION,
                );
            }
            Err(error) => show_error(window, &error),
        }
    }

    unsafe fn show_error(window: HWND, error: &str) {
        MessageBoxW(
            window,
            wide(error).as_ptr(),
            wide("操作失败").as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }

    fn format_install_details(state: &UiState) -> String {
        if let Some(error) = state.install_error.as_deref() {
            return format!("状态检测失败\r\n\r\n{error}");
        }
        let Some(report) = state.install_report.as_ref() else {
            return "正在检测安装状态...".to_owned();
        };
        let real_cli = report
            .real_cli
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| {
                format!(
                    "未找到 ({})",
                    report.real_cli_error.as_deref().unwrap_or("未知错误")
                )
            });
        format!(
            "官方 Codex CLI    {real_cli}\r\n\
             安装目录          {}\r\n\
             CODEX_CLI_PATH    {}\r\n\r\n\
             启动器            {}\r\n\
             图片代理          {}\r\n\
             命令别名          {}\r\n\
             环境集成          {}\r\n\
             Alias 集成        {}\r\n\
             代理完整性        {}",
            report.fix_root.display(),
            report.codex_cli_path.as_deref().unwrap_or("未设置"),
            present(report.launcher_present),
            present(report.proxy_present),
            present(report.alias_present),
            health(report.environment_healthy),
            health(report.alias_healthy),
            health(report.proxy_healthy),
        )
    }

    fn format_diagnostics(state: &UiState) -> String {
        let mut text = String::new();
        if let Some(report) = state.install_report.as_ref() {
            text.push_str(&format!(
                "[图片兼容层]\r\n状态              {}\r\n安装目录          {}\r\n官方 Codex CLI    {}\r\nCODEX_CLI_PATH    {}\r\n\r\n",
                match report.health { InstallationHealth::Healthy => "正常", InstallationHealth::NotInstalled => "未安装", InstallationHealth::Broken => "异常" },
                report.fix_root.display(),
                report.real_cli.as_ref().map(|path| path.display().to_string()).unwrap_or_else(|| "未找到".to_owned()),
                report.codex_cli_path.as_deref().unwrap_or("未设置"),
            ));
        } else if let Some(error) = state.install_error.as_deref() {
            text.push_str(&format!(
                "[图片兼容层]\r\n检测失败          {error}\r\n\r\n"
            ));
        }
        if let Some(settings) = state.model_settings.as_ref() {
            text.push_str(&format!(
                "[模型配置]\r\nCodex Home        {}\r\nconfig.toml       {}\r\nauth.json         {}\r\nProvider          {}\r\n服务器地址        {}\r\n图片模型 ID       {}\r\n图片生成          {}\r\n静态 Header       {} 项（值已隐藏）\r\n环境 Header       {} 项（值未读取）\r\n受管理备份        {}\r\nAPI Key           {}",
                settings.codex_home.display(),
                settings.config_path.display(),
                settings.auth_path.display(),
                settings.provider_id,
                if settings.server_url.is_empty() { "未配置" } else { &settings.server_url },
                settings.image_model,
                if settings.image_model_enabled { "已启用" } else { "未启用" },
                settings.static_headers.len(),
                settings.env_headers.len(),
                if settings.managed { "存在" } else { "无" },
                if settings.api_key.is_empty() { "未配置" } else { "已配置（已隐藏）" },
            ));
        } else if let Some(error) = state.model_error.as_deref() {
            text.push_str(&format!("[模型配置]\r\n检测失败          {error}"));
        }
        let processes = diagnostics::running_codex_processes().unwrap_or_default();
        text.push_str("\r\n\r\n[Codex 进程]\r\n");
        if processes.is_empty() {
            text.push_str("当前未检测到 Codex Desktop/Codex++ 进程");
        } else {
            for process in processes {
                text.push_str(&format!(
                    "PID {:<8} {}\r\n",
                    process.process_id, process.executable
                ));
            }
        }
        text
    }

    fn header_health(state: &UiState) -> (&str, COLORREF) {
        if state.busy {
            return (&state.busy_text, COLOR_TEAL);
        }
        match state.install_report.as_ref().map(|report| report.health) {
            Some(InstallationHealth::Healthy) => ("运行正常", COLOR_GREEN),
            Some(InstallationHealth::NotInstalled) => ("等待安装", COLOR_MUTED),
            Some(InstallationHealth::Broken) => ("需要修复", COLOR_AMBER),
            None if state.install_error.is_some() => ("检测失败", COLOR_RED),
            None => ("正在检测", COLOR_MUTED),
        }
    }

    fn install_status(state: &UiState) -> &str {
        match state.install_report.as_ref().map(|report| report.health) {
            Some(InstallationHealth::Healthy) => "已安装",
            Some(InstallationHealth::NotInstalled) => "尚未安装",
            Some(InstallationHealth::Broken) => "安装异常",
            None => "正在检测",
        }
    }

    fn install_status_color(state: &UiState) -> COLORREF {
        match state.install_report.as_ref().map(|report| report.health) {
            Some(InstallationHealth::Healthy) => COLOR_GREEN,
            Some(InstallationHealth::Broken) => COLOR_AMBER,
            _ => COLOR_MUTED,
        }
    }

    unsafe fn draw_dot(hdc: HDC, center_x: i32, center_y: i32, color: COLORREF) {
        let brush = CreateSolidBrush(color);
        let pen = CreatePen(PS_SOLID, 1, color);
        let old_brush = SelectObject(hdc, brush as _);
        let old_pen = SelectObject(hdc, pen as _);
        Ellipse(hdc, center_x - 5, center_y - 5, center_x + 5, center_y + 5);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        DeleteObject(pen as _);
        DeleteObject(brush as _);
    }

    unsafe fn draw_check(
        hdc: HDC,
        state: &UiState,
        center_x: i32,
        center_y: i32,
        healthy: bool,
        known: bool,
    ) {
        let (fill_color, text_color, symbol) = if !known {
            (rgb(235, 238, 241), COLOR_MUTED, "·")
        } else if healthy {
            (COLOR_GREEN_PALE, COLOR_GREEN, "✓")
        } else {
            (rgb(248, 224, 224), COLOR_RED, "!")
        };
        let radius = scale(state, 9);
        let brush = CreateSolidBrush(fill_color);
        let pen = CreatePen(PS_SOLID, 1, fill_color);
        let old_brush = SelectObject(hdc, brush as _);
        let old_pen = SelectObject(hdc, pen as _);
        Ellipse(
            hdc,
            center_x - radius,
            center_y - radius,
            center_x + radius,
            center_y + radius,
        );
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        DeleteObject(pen as _);
        DeleteObject(brush as _);
        draw_text(
            hdc,
            symbol,
            RECT {
                left: center_x - radius,
                top: center_y - radius,
                right: center_x + radius,
                bottom: center_y + radius,
            },
            state.fonts.small_bold,
            text_color,
            TEXT_CENTER,
        );
    }

    unsafe fn rounded_rect(
        hdc: HDC,
        rect: RECT,
        fill_color: COLORREF,
        border_color: COLORREF,
        radius: i32,
    ) {
        let brush = CreateSolidBrush(fill_color);
        let pen = CreatePen(PS_SOLID, 1, border_color);
        let old_brush = SelectObject(hdc, brush as _);
        let old_pen = SelectObject(hdc, pen as _);
        RoundRect(
            hdc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            radius * 2,
            radius * 2,
        );
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        DeleteObject(pen as _);
        DeleteObject(brush as _);
    }

    unsafe fn fill(hdc: HDC, rect: RECT, color: COLORREF) {
        let brush = CreateSolidBrush(color);
        FillRect(hdc, &rect, brush);
        DeleteObject(brush as _);
    }

    unsafe fn draw_text(
        hdc: HDC,
        text: &str,
        mut rect: RECT,
        font: HFONT,
        color: COLORREF,
        format: u32,
    ) {
        let old_font = SelectObject(hdc, font as _);
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, color);
        let text = wide(text);
        DrawTextW(hdc, text.as_ptr(), -1, &mut rect, format);
        SelectObject(hdc, old_font);
    }

    unsafe fn read_text(control: HWND) -> String {
        let length = GetWindowTextLengthW(control).max(0) as usize;
        let mut buffer = vec![0u16; length + 1];
        let copied =
            GetWindowTextW(control, buffer.as_mut_ptr(), buffer.len() as i32).max(0) as usize;
        String::from_utf16_lossy(&buffer[..copied])
    }

    unsafe fn set_text(control: HWND, text: &str) {
        SetWindowTextW(control, wide(text).as_ptr());
    }

    unsafe fn state(window: HWND) -> Option<&'static UiState> {
        (GetWindowLongPtrW(window, GWLP_USERDATA) as *const UiState).as_ref()
    }

    unsafe fn state_mut(window: HWND) -> Option<&'static mut UiState> {
        (GetWindowLongPtrW(window, GWLP_USERDATA) as *mut UiState).as_mut()
    }

    fn scale(state: &UiState, value: i32) -> i32 {
        scale_for(state.layout_dpi, value)
    }

    fn scale_for(dpi: u32, value: i32) -> i32 {
        ((value as i64 * dpi as i64 + 48) / 96) as i32
    }

    fn logical_rect(state: &UiState, left: i32, top: i32, width: i32, height: i32) -> RECT {
        RECT {
            left: scale(state, left),
            top: scale(state, top),
            right: scale(state, left + width),
            bottom: scale(state, top + height),
        }
    }

    fn inset_rect(rect: RECT, amount: i32) -> RECT {
        RECT {
            left: rect.left + amount,
            top: rect.top + amount,
            right: rect.right - amount,
            bottom: rect.bottom - amount,
        }
    }

    fn margin_parameter(left: i32, right: i32) -> isize {
        (((right as u32) << 16) | (left as u32 & 0xffff)) as isize
    }

    fn button_label(id: i32) -> &'static str {
        match id {
            ID_INSTALL => "安装 / 更新",
            ID_UNINSTALL => "卸载",
            ID_INSTALL_REFRESH => "刷新状态",
            ID_TEST_CONNECTION => "测试连接",
            ID_RESTORE_CONFIG => "恢复配置",
            ID_SAVE_CONFIG => "保存并启用",
            ID_DIAGNOSTICS_REFRESH => "重新检测",
            ID_DIAGNOSTICS_EXPORT => "导出诊断",
            ID_LAUNCH_CODEX => "启动 Codex",
            _ => "",
        }
    }

    fn tone_color(tone: MessageTone) -> COLORREF {
        match tone {
            MessageTone::Good => COLOR_GREEN,
            MessageTone::Error => COLOR_RED,
            MessageTone::Muted => COLOR_MUTED,
        }
    }

    fn format_error(error: anyhow::Error) -> String {
        format!("{error:#}")
    }

    fn present(value: bool) -> &'static str {
        if value {
            "存在"
        } else {
            "缺失"
        }
    }

    fn health(value: bool) -> &'static str {
        if value {
            "正常"
        } else {
            "异常"
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    const TEXT_LEFT: u32 = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS;
    const TEXT_CENTER: u32 = DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX;
    const DT_RIGHT_FLAGS: u32 = 2 | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS;

    const fn rgb(red: u32, green: u32, blue: u32) -> COLORREF {
        red | (green << 8) | (blue << 16)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rapid_page_requests_coalesce_to_the_last_target() {
            let mut queue = PageSwitchQueue::default();
            let mut posted_messages = 0;
            for index in 0..300 {
                let page = match index % 3 {
                    0 => Page::Install,
                    1 => Page::Model,
                    _ => Page::Diagnostics,
                };
                if queue.request(page) {
                    posted_messages += 1;
                }
            }

            assert_eq!(posted_messages, 1);
            assert_eq!(queue.take(), Some(Page::Diagnostics));
            assert!(!queue.message_pending);
        }
    }
}

#[cfg(windows)]
pub use windows::{run, show_fatal_error};

#[cfg(not(windows))]
pub fn show_fatal_error(_message: &str) {}
