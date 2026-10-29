(function () {
  const escapeToken = __CLIPHOST_ESCAPE_TOKEN__;
  if (window.__clipHostWindowState?.escapeToken !== escapeToken) return;
  try {
    const api = window.__clipHostFakeFocusSearch;
    if (api?.hasEscapeLayer?.()) return;
    if (api && typeof api.handleEscape === "function" && api.handleEscape()) return;
  } catch (_) {}

  // The page handles Escape first (e.g. dismiss an editor or clear search).
  // An unhandled key asks the host to hide without blocking the renderer on HTTP.
  const port = window.__INTELLITOOLS_LOCAL_API_PORT || 28965;
  void fetch("http://127.0.0.1:" + port + "/api/clip/host-window", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ v: 1, kind: "clipHostWindow", host: "clipboard", action: "escapeUnhandled",
      escapeToken, escapeInputToken: __CLIPHOST_ESCAPE_INPUT_TOKEN__ })
  }).catch(() => {});
})();
