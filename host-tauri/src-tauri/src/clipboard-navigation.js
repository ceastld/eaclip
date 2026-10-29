(function () {
  try {
    const api = window.__clipHostFakeFocusSearch;
    if (api && typeof api.dispatchNavigationKey === 'function') {
      api.dispatchNavigationKey(__CLIPHOST_NAV_KEY__, __CLIPHOST_NAV_SHIFT__);
    }
  } catch (_) {}
})();
