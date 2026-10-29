(function () {
  const boot = window.__INTELLITOOLS_HOST_BOOT__;
  const label = boot?.sourceWindowLabel;
  if (label !== "clipboard" && label !== "panel") return;

  // The toolbar reports its own modal state separately. Include editors, rename
  // dialogs and context menus, but exclude layers in an inactive KeepAlive tab.
  let desired = false;
  let sent;
  let sending = false;
  let retry;
  function visibleLayer() {
    if (document.visibilityState === "hidden") return false;
    return Array.from(document.querySelectorAll('[role="dialog"], [role="menu"], [aria-modal="true"], [data-clip-portal-overlay]'))
      .some(function (element) {
        if (element.closest('[hidden], [aria-hidden="true"]') || !element.getClientRects().length) return false;
        const style = getComputedStyle(element);
        return style.visibility !== "hidden" && style.visibility !== "collapse" && style.display !== "none";
      });
  }
  function endpoint() {
    const dev = (location.hostname === "127.0.0.1" || location.hostname === "localhost")
      && location.port && location.port !== String(boot.apiPort);
    return dev ? "/__tauri-cliphost/api/clip/host-window"
      : "http://127.0.0.1:" + boot.apiPort + "/api/clip/host-window";
  }
  async function flush() {
    if (sending || retry !== undefined || sent === desired) return;
    sending = true;
    try {
      // Serialize requests: a slow open must not overwrite a newer close.
      while (sent !== desired) {
        const value = desired;
        const response = await fetch(endpoint(), {
          method: "POST", headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ v: 1, kind: "clipHostWindow", host: label,
            sourceHostWindow: label, action: "setSpaOverlayOpen", value })
        });
        if (!response.ok) throw new Error("Overlay state HTTP " + response.status);
        sent = value;
      }
    } catch (_) {
      // Initialization can precede the HTTP listener. Retry the latest state.
      retry = window.setTimeout(function () { retry = undefined; void flush(); }, 500);
    } finally {
      sending = false;
    }
  }
  function sync() { desired = visibleLayer(); void flush(); }
  function start() {
    new MutationObserver(sync).observe(document.documentElement, {
      childList: true, subtree: true, attributes: true,
      attributeFilter: ["hidden", "aria-hidden", "role", "aria-modal", "class", "style", "data-clip-portal-overlay"]
    });
    document.addEventListener("visibilitychange", sync);
    sync();
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start, { once: true });
  else start();
})();
