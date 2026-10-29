(function () {
  try {
    const api = window.__clipHostFakeFocusSearch;
    if (api && typeof api.routeDigitKey === 'function') {
      api.routeDigitKey(__CLIPHOST_DIGIT__, __CLIPHOST_DIGIT_REPEAT__);
    }
  } catch (_) {}
})();
