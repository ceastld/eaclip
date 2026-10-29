(function () {
  const apiPort = __CLIPHOST_BOOT_PORT__;
  const initialRoute = __CLIPHOST_BOOT_ROUTE__;
  const sourceWindowLabel = __CLIPHOST_BOOT_WINDOW_LABEL__;
  window.__INTELLITOOLS_HOST_BOOT__ = {
    apiPort, sourceWindowLabel, sourceWindowHandle: "0", initialSpaRoute: initialRoute, hostKind: "tauri"
  };
  window.__INTELLITOOLS_LOCAL_API_PORT = apiPort;
  window.__CLIPHOST_FORCE_HTTP_RPC__ = true;
  // Pushes arrive through Rust's WebView eval, including in Vite development.
  // Open WebViews must not occupy the HTTP/1.1 connection pool with SSE.
  window.__CLIPHOST_NATIVE_PUSH__ = true;
  window.__clipHostPushQueue = window.__clipHostPushQueue || [];

  function markHost() {
    if (!document.documentElement) return;
    document.documentElement.classList.add("clip-host-tauri");
    document.documentElement.setAttribute("data-cliphost-host", "tauri");
  }
  markHost();
  document.addEventListener("DOMContentLoaded", markHost, { once: true });

  // Keep chrome.webview intact: WRY uses its native postMessage for Tauri IPC.
  // These host-specific operations are separate from WebView2's COM host objects.
  function apiUrl(path) {
    const dev = (location.hostname === "127.0.0.1" || location.hostname === "localhost")
      && location.port && location.port !== String(apiPort);
    return dev ? "/__tauri-cliphost" + path : "http://127.0.0.1:" + apiPort + path;
  }
  window.__clipHostNative = {
    beginOleDrag(ids) {
      return fetch(apiUrl("/api/clip/ole-drag"), {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ ids })
      }).then(async (response) => {
        if (!response.ok) {
          const body = await response.json().catch(() => null);
          throw new Error(body?.error || "OLE drag HTTP " + response.status);
        }
      });
    }
  };
  // Focused WebViews receive DOM keys, unlike the no-focus LL-hook route.
  // Snapshot page layers during capture so closing a dialog on this same key
  // cannot also hide its parent window. Let IME and page handlers run first.
  if (initialRoute === "/clipboard") {
    document.addEventListener("keydown", function (event) {
      if (event.key !== "Escape" || !event.isTrusted || event.isComposing || event.keyCode === 229
          || event.repeat || event.ctrlKey || event.altKey || event.metaKey || event.shiftKey) return;
      const api = window.__clipHostFakeFocusSearch;
      const escapeToken = window.__clipHostWindowState?.escapeToken;
      if (!api || api.hasEscapeLayer?.() || typeof escapeToken !== "string" || !escapeToken) return;
      window.setTimeout(function () {
        if (event.defaultPrevented || event.cancelBubble || document.visibilityState === "hidden") return;
        if (window.__clipHostWindowState?.escapeToken !== escapeToken) return;
        if (window.__clipHostFakeFocusSearch !== api || api.hasEscapeLayer?.() || api.handleEscape?.()) return;
        void fetch(apiUrl("/api/clip/host-window"), {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ v: 1, kind: "clipHostWindow", host: "clipboard", action: "escapeUnhandled", escapeToken })
        }).catch(function (error) { console.error("ClipHost Escape failed", error); });
      }, 0);
    }, true);
  }
  // Native navigation can finish before React registers its router. Keep only
  // the latest request in this document instead of starting another page load.
  let pendingRoute = null;
  window.__clipHostDrainRoute = function () {
    if (pendingRoute === null || typeof window.__clipHostNavigate !== "function") return;
    const route = pendingRoute;
    pendingRoute = null;
    window.__clipHostNavigate(route);
  };
  window.__clipHostAssignRoute = function (route) {
    window.__clipHostHoverPopup?.hide(true);
    if (typeof route !== "string" || !route.startsWith("/") || route.startsWith("//")) return;
    const dest = new URL(route, location.href);
    if (dest.origin !== location.origin) return;
    if (typeof window.__clipHostNavigate === "function") {
      pendingRoute = null;
      window.__clipHostNavigate(route);
      return;
    }
    pendingRoute = dest.pathname + dest.search + dest.hash;
  };
})();
