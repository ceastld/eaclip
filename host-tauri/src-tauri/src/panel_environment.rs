//! Give only the panel an in-process WebView2 input/drop target. Tauri exposes
//! an environment override, but not controller creation options. This adapter
//! delegates to the existing environment and sets the supported option before
//! Wry creates the controller. No browser-process pointers or patched Wry fork.
use std::cell::RefCell;
use tauri::Manager;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use windows61::{
    core::*,
    Win32::{Foundation::HWND, System::Com::IStream},
};

thread_local! {
    static PREPARED: RefCell<Option<Result<ICoreWebView2Environment>>> = const { RefCell::new(None) };
}

/// Called during window creation on Tauri's UI thread. with_webview dispatches
/// inline there; never block that thread waiting for a scheduled callback.
pub fn for_panel(app: &tauri::AppHandle) -> std::result::Result<ICoreWebView2Environment, String> {
    let source = app
        .get_webview_window("clipboard")
        .ok_or("Clipboard WebView is not initialized")?;
    PREPARED.with(|slot| {
        slot.borrow_mut().take();
    });
    source
        .with_webview(|view| {
            let prepared = (|| {
                let inner = view.environment().cast::<ICoreWebView2Environment10>()?;
                // Fail before changing any drop registration if the runtime is too old.
                unsafe {
                    inner
                        .CreateCoreWebView2ControllerOptions()?
                        .cast::<ICoreWebView2ControllerOptions4>()?
                        .SetAllowHostInputProcessing(true)?;
                }
                let adapter: ICoreWebView2Environment10 = PanelEnvironment(inner).into();
                adapter.cast()
            })();
            PREPARED.with(|slot| *slot.borrow_mut() = Some(prepared));
        })
        .map_err(|e| e.to_string())?;
    PREPARED
        .with(|slot| slot.borrow_mut().take())
        .ok_or("Panel environment must be prepared on the UI thread")?
        .map_err(|e| e.to_string())
}

#[implement(ICoreWebView2Environment10)]
struct PanelEnvironment(ICoreWebView2Environment10);

// Keep the environment's resource handling, browser events, profile and requests
// intact. Only controller creation changes; composition controllers pass through.
#[allow(non_snake_case)]
impl ICoreWebView2Environment_Impl for PanelEnvironment_Impl {
    fn CreateCoreWebView2Controller(
        &self,
        parent: HWND,
        handler: Ref<'_, ICoreWebView2CreateCoreWebView2ControllerCompletedHandler>,
    ) -> Result<()> {
        let options = self.CreateCoreWebView2ControllerOptions()?;
        unsafe {
            self.0
                .CreateCoreWebView2ControllerWithOptions(parent, &options, handler.as_ref())
        }
    }
    fn CreateWebResourceResponse(
        &self,
        content: Ref<'_, IStream>,
        status: i32,
        reason: &PCWSTR,
        headers: &PCWSTR,
    ) -> Result<ICoreWebView2WebResourceResponse> {
        unsafe {
            self.0
                .CreateWebResourceResponse(content.as_ref(), status, *reason, *headers)
        }
    }
    fn BrowserVersionString(&self, value: *mut PWSTR) -> Result<()> {
        unsafe { self.0.BrowserVersionString(value) }
    }
    fn add_NewBrowserVersionAvailable(
        &self,
        handler: Ref<'_, ICoreWebView2NewBrowserVersionAvailableEventHandler>,
        token: *mut i64,
    ) -> Result<()> {
        unsafe {
            self.0
                .add_NewBrowserVersionAvailable(handler.as_ref(), token)
        }
    }
    fn remove_NewBrowserVersionAvailable(&self, token: i64) -> Result<()> {
        unsafe { self.0.remove_NewBrowserVersionAvailable(token) }
    }
}
#[allow(non_snake_case)]
impl ICoreWebView2Environment2_Impl for PanelEnvironment_Impl {
    fn CreateWebResourceRequest(
        &self,
        uri: &PCWSTR,
        method: &PCWSTR,
        post: Ref<'_, IStream>,
        headers: &PCWSTR,
    ) -> Result<ICoreWebView2WebResourceRequest> {
        unsafe {
            self.0
                .CreateWebResourceRequest(*uri, *method, post.as_ref(), *headers)
        }
    }
}
#[allow(non_snake_case)]
impl ICoreWebView2Environment3_Impl for PanelEnvironment_Impl {
    fn CreateCoreWebView2CompositionController(
        &self,
        parent: HWND,
        handler: Ref<'_, ICoreWebView2CreateCoreWebView2CompositionControllerCompletedHandler>,
    ) -> Result<()> {
        unsafe {
            self.0
                .CreateCoreWebView2CompositionController(parent, handler.as_ref())
        }
    }
    fn CreateCoreWebView2PointerInfo(&self) -> Result<ICoreWebView2PointerInfo> {
        unsafe { self.0.CreateCoreWebView2PointerInfo() }
    }
}
#[allow(non_snake_case)]
impl ICoreWebView2Environment4_Impl for PanelEnvironment_Impl {
    fn GetAutomationProviderForWindow(&self, hwnd: HWND) -> Result<IUnknown> {
        unsafe { self.0.GetAutomationProviderForWindow(hwnd) }
    }
}
#[allow(non_snake_case)]
impl ICoreWebView2Environment5_Impl for PanelEnvironment_Impl {
    fn add_BrowserProcessExited(
        &self,
        handler: Ref<'_, ICoreWebView2BrowserProcessExitedEventHandler>,
        token: *mut i64,
    ) -> Result<()> {
        unsafe { self.0.add_BrowserProcessExited(handler.as_ref(), token) }
    }
    fn remove_BrowserProcessExited(&self, token: i64) -> Result<()> {
        unsafe { self.0.remove_BrowserProcessExited(token) }
    }
}
#[allow(non_snake_case)]
impl ICoreWebView2Environment6_Impl for PanelEnvironment_Impl {
    fn CreatePrintSettings(&self) -> Result<ICoreWebView2PrintSettings> {
        unsafe { self.0.CreatePrintSettings() }
    }
}
#[allow(non_snake_case)]
impl ICoreWebView2Environment7_Impl for PanelEnvironment_Impl {
    fn UserDataFolder(&self, value: *mut PWSTR) -> Result<()> {
        unsafe { self.0.UserDataFolder(value) }
    }
}
#[allow(non_snake_case)]
impl ICoreWebView2Environment8_Impl for PanelEnvironment_Impl {
    fn add_ProcessInfosChanged(
        &self,
        handler: Ref<'_, ICoreWebView2ProcessInfosChangedEventHandler>,
        token: *mut i64,
    ) -> Result<()> {
        unsafe { self.0.add_ProcessInfosChanged(handler.as_ref(), token) }
    }
    fn remove_ProcessInfosChanged(&self, token: i64) -> Result<()> {
        unsafe { self.0.remove_ProcessInfosChanged(token) }
    }
    fn GetProcessInfos(&self) -> Result<ICoreWebView2ProcessInfoCollection> {
        unsafe { self.0.GetProcessInfos() }
    }
}
#[allow(non_snake_case)]
impl ICoreWebView2Environment9_Impl for PanelEnvironment_Impl {
    fn CreateContextMenuItem(
        &self,
        label: &PCWSTR,
        icon: Ref<'_, IStream>,
        kind: COREWEBVIEW2_CONTEXT_MENU_ITEM_KIND,
    ) -> Result<ICoreWebView2ContextMenuItem> {
        unsafe { self.0.CreateContextMenuItem(*label, icon.as_ref(), kind) }
    }
}
#[allow(non_snake_case)]
impl ICoreWebView2Environment10_Impl for PanelEnvironment_Impl {
    fn CreateCoreWebView2ControllerOptions(&self) -> Result<ICoreWebView2ControllerOptions> {
        unsafe {
            let options = self.0.CreateCoreWebView2ControllerOptions()?;
            options
                .cast::<ICoreWebView2ControllerOptions4>()?
                .SetAllowHostInputProcessing(true)?;
            Ok(options)
        }
    }
    fn CreateCoreWebView2ControllerWithOptions(
        &self,
        parent: HWND,
        options: Ref<'_, ICoreWebView2ControllerOptions>,
        handler: Ref<'_, ICoreWebView2CreateCoreWebView2ControllerCompletedHandler>,
    ) -> Result<()> {
        unsafe {
            options
                .ok()?
                .cast::<ICoreWebView2ControllerOptions4>()?
                .SetAllowHostInputProcessing(true)?;
            self.0.CreateCoreWebView2ControllerWithOptions(
                parent,
                options.as_ref(),
                handler.as_ref(),
            )
        }
    }
    fn CreateCoreWebView2CompositionControllerWithOptions(
        &self,
        parent: HWND,
        options: Ref<'_, ICoreWebView2ControllerOptions>,
        handler: Ref<'_, ICoreWebView2CreateCoreWebView2CompositionControllerCompletedHandler>,
    ) -> Result<()> {
        unsafe {
            self.0.CreateCoreWebView2CompositionControllerWithOptions(
                parent,
                options.as_ref(),
                handler.as_ref(),
            )
        }
    }
}
