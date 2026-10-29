(function () {
  try {
    if (document.hidden) return;
    var active = document.activeElement;
    if (active && active.closest("input, textarea, select, [contenteditable]:not([contenteditable='false']), .cm-editor")) return;
    var layers = document.querySelectorAll('[role="dialog"], [role="menu"], [aria-modal="true"], [data-clip-portal-overlay]');
    if (Array.from(layers).some(function (element) {
      if (element.closest('[hidden], [aria-hidden="true"]') || !element.getClientRects().length) return false;
      var style = getComputedStyle(element);
      return style.visibility !== "hidden" && style.visibility !== "collapse" && style.display !== "none";
    })) return;
    var root = document.querySelector('.panel-page') || document.querySelector('.panel-overview');
    if (!root) return;
    var key = __CLIPHOST_PANEL_KEY__;
    if (typeof root.focus === 'function') root.focus();
    root.dispatchEvent(new KeyboardEvent('keydown', {
      key: key, code: key === ' ' ? 'Space' : key,
      bubbles: true, cancelable: true, composed: true
    }));
  } catch (_) {}
})();
