// Native-only capability. The shared SPA keeps its DOM previews in other hosts.
(function () {
  const HOVER_DELAY_MS = 500;
  let ticket = Date.now() * 1000;
  let active = null;
  let timer = 0;
  let nativeMayBeVisible = false;
  let visibleTarget = null;
  function invoke(payload) {
    return window.__TAURI_INTERNALS__.invoke("set_hover_preview", { payload: { ...payload, ticket: ++ticket } });
  }
  function dark() {
    const color = getComputedStyle(document.documentElement).backgroundColor.match(/\d+/g);
    return !color || (+color[0] + +color[1] + +color[2]) < 384;
  }
  function restoreTitle(item) {
    const target = item?.titleTarget ?? item?.target;
    if (item?.title != null && target.getAttribute("title") === "") {
      target.setAttribute("title", item.title);
    }
  }
  function hide(force = false, expected = active) {
    if (expected !== active) return;
    clearTimeout(timer); timer = 0;
    restoreTitle(active); active = null;
    if (!nativeMayBeVisible) return;
    if (force) { nativeMayBeVisible = false; visibleTarget = null; }
    void invoke({ action: "hide", force }).catch(error => console.warn("Hover popup hide failed", error));
  }
  function scheduleShow(item) {
    clearTimeout(timer);
    timer = setTimeout(() => {
      if (active !== item) return;
      timer = 0;
      const { target, payload, onError } = item;
      const rect = target.getBoundingClientRect();
      if (!target.isConnected || rect.width <= 0 || rect.height <= 0) { hide(true, item); return; }
      item.shown = true;
      nativeMayBeVisible = true;
      visibleTarget = target;
      void invoke({ ...payload, anchor: [rect.left, rect.top, rect.width, rect.height], dark: dark() }).catch(error => {
        if (active !== item) return;
        hide(true, item); onError?.(error);
      });
    }, HOVER_DELAY_MS);
  }
  function show(target, payload, onError, title) {
    hide(true);
    const titleTarget = title != null ? target : target.closest("[title]");
    const originalTitle = title ?? titleTarget?.getAttribute("title");
    const item = { target, payload, onError, title: originalTitle, titleTarget, shown: false };
    active = item;
    const rect = target.getBoundingClientRect();
    if (!target.isConnected || rect.width <= 0 || rect.height <= 0) { hide(true); return () => {}; }
    // Suppress Chromium's second tooltip throughout the native delay. Restore
    // only our empty attribute so newer React titles remain untouched.
    if (originalTitle != null) titleTarget.setAttribute("title", "");
    scheduleShow(item);
    return (force = false) => hide(force, item);
  }
  window.__clipHostHoverPopup = {
    showImage(target, imageSrc, metaLine, onError) { return show(target, { action: "image", imageSrc, metaLine }, onError); },
    showText(target, text, onError) { return show(target, { action: "text", text }, onError); },
    hide
  };
  document.addEventListener("mouseover", event => {
    if (active?.payload.action === "image") {
      // React retains its image-open state for a short leave grace. Re-entering
      // that same thumbnail may not run its effect again, so re-arm our timer.
      if (!active.shown && !timer && event.target instanceof Node && active.target.contains(event.target)) {
        scheduleShow(active);
      }
      return;
    }
    const target = event.target instanceof Element ? event.target.closest("[title]") : null;
    if (!target || (active?.target === target)) return;
    const title = target.getAttribute("title");
    if (!title?.trim()) return;
    show(target, { action: "text", text: title }, error => console.warn("Hover popup text failed", error), title);
  }, true);
  document.addEventListener("mouseout", event => {
    if (!active) return;
    if (event.relatedTarget instanceof Node && active.target.contains(event.relatedTarget)) return;
    if (active.payload.action === "image") {
      if (!active.shown) { clearTimeout(timer); timer = 0; }
      return;
    }
    hide(false);
  }, true);
  document.addEventListener("pointerdown", () => hide(true), true);
  document.addEventListener("contextmenu", () => hide(true), true);
  document.addEventListener("keydown", () => hide(true), true);
  document.addEventListener("scroll", () => hide(true), true);
  window.addEventListener("resize", () => hide(true));
  window.addEventListener("pagehide", () => hide(true));
  document.addEventListener("visibilitychange", () => { if (document.hidden) hide(true); });
  new MutationObserver(() => {
    if ((active && !active.target.isConnected) || (visibleTarget && !visibleTarget.isConnected)) hide(true);
  }).observe(document, { childList: true, subtree: true });
})();
