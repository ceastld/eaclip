(function () {
  const invoke = function (command, args) {
    const api = window.__TAURI__?.core?.invoke || window.__TAURI_INTERNALS__?.invoke;
    return api ? api(command, args || {}) : Promise.reject(new Error("Native menu unavailable"));
  };
  window.__clipHostContextMenuPopup = {
    open: (payload) => invoke("context_menu_open", { payload }),
    close: (sessionId) => invoke("context_menu_close", { sessionId }),
    choose: (sessionId, key) => invoke("context_menu_select", { sessionId, key }),
    fit: (sessionId, width, height) => invoke("context_menu_resize", { sessionId, width, height }),
    ready: () => invoke("context_menu_ready")
  };
})();
