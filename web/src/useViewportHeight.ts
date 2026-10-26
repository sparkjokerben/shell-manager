import { useEffect } from "react";

/**
 * 把 iOS visualViewport 的可见区域写入 CSS 变量。
 *
 * iOS Safari 在软键盘弹出时不会缩小布局视口，仅缩小 visualViewport，
 * 因此 `position: fixed` 的元素会被键盘遮住。这里把可见高度和偏移写入
 * `--app-height` / `--app-top`，供 `.app` 及固定定位的浮层使用。
 */
export function useViewportHeight() {
  useEffect(() => {
    const root = document.documentElement;
    const viewport = window.visualViewport;
    let frame: number | null = null;

    const apply = () => {
      frame = null;
      const height = viewport ? viewport.height : window.innerHeight;
      const top = viewport ? viewport.offsetTop : 0;
      root.style.setProperty("--app-height", `${Math.max(0, Math.round(height))}px`);
      root.style.setProperty("--app-top", `${Math.max(0, Math.round(top))}px`);
    };

    const schedule = () => {
      if (frame !== null) {
        return;
      }
      frame = window.requestAnimationFrame(apply);
    };

    apply();
    viewport?.addEventListener("resize", schedule);
    viewport?.addEventListener("scroll", schedule);
    window.addEventListener("resize", schedule);
    window.addEventListener("orientationchange", schedule);

    return () => {
      if (frame !== null) {
        window.cancelAnimationFrame(frame);
      }
      viewport?.removeEventListener("resize", schedule);
      viewport?.removeEventListener("scroll", schedule);
      window.removeEventListener("resize", schedule);
      window.removeEventListener("orientationchange", schedule);
      root.style.removeProperty("--app-height");
      root.style.removeProperty("--app-top");
    };
  }, []);
}
