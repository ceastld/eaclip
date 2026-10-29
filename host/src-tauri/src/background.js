// Keep the native erase brush and WebView2's default background in step with
// the page. CSS alone cannot paint the newly exposed area during live resize.
(function () {
  let lastApplied = "";
  let running = false;
  let dirty = false;
  let cornerFrame = 0;
  let wallpaper = "";

  function refreshCorners() {
    if (cornerFrame) return;
    cornerFrame = requestAnimationFrame(() => {
      cornerFrame = 0;
      window.__TAURI_INTERNALS__.invoke("refresh_host_corners").catch(() => {});
      const image = getComputedStyle(document.documentElement, "::before").backgroundImage;
      if (image !== wallpaper) {
        wallpaper = image;
        const url = image.match(/^url\(["']?(.*?)["']?\)$/);
        if (url) {
          const preload = new Image();
          preload.onload = refreshCorners;
          preload.src = url[1];
        }
      }
    });
  }

  async function syncBackground() {
    refreshCorners();
    dirty = true;
    if (running) return;
    running = true;
    try {
      while (dirty) {
        dirty = false;
        if (!document.body) continue;
        // The browser resolves preset, system and custom palettes for us.
        // Body may be transparent when a wallpaper is enabled; the root canvas
        // still resolves --bg-page to the correct solid fallback.
        const color = getComputedStyle(document.documentElement).backgroundColor;
        const channels = color.match(/^rgba?\(\s*(\d+)\s*[, ]\s*(\d+)\s*[, ]\s*(\d+)/);
        if (!channels || color === lastApplied) continue;
        const rgb = channels.slice(1, 4).map(Number);
        await window.__TAURI_INTERNALS__.invoke("set_host_background", { rgb });
        lastApplied = color;
      }
    } catch (error) {
      // A failed call can retry on the next theme change; never block resize.
      console.warn("ClipHost native background sync failed", error);
    } finally {
      running = false;
    }
  }

  function install() {
    const root = document.documentElement;
    // The HTML boot style is dark; keep the root canvas in step with body too.
    root.style.backgroundColor = "var(--bg-page, #1b1b1b)";
    new MutationObserver(syncBackground).observe(root, {
      attributes: true, attributeFilter: ["data-theme", "data-system-theme", "style", "class"]
    });
    window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", syncBackground);
    window.addEventListener("load", syncBackground, { once: true });
    window.addEventListener("resize", refreshCorners);
    syncBackground();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", install, { once: true });
  } else {
    install();
  }
})();
