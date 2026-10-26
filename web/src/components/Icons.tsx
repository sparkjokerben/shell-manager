import type { SVGProps } from "react";

type IconProps = SVGProps<SVGSVGElement>;

export function MenuIcon(props: IconProps) {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" focusable="false" {...props}>
      <path d="M2 4.25h12M2 8h12M2 11.75h8" fill="none" stroke="currentColor" strokeWidth="1.4" />
    </svg>
  );
}

export function CloseIcon(props: IconProps) {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" focusable="false" {...props}>
      <path d="m4 4 8 8M12 4l-8 8" fill="none" stroke="currentColor" strokeWidth="1.4" />
    </svg>
  );
}

export function RefreshIcon(props: IconProps) {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" focusable="false" {...props}>
      <path
        d="M13.2 6.2A5.4 5.4 0 0 0 3.1 5.1L2 6.5M2 6.5V2.9M2 6.5h3.6M2.8 9.8a5.4 5.4 0 0 0 10.1 1.1l1.1-1.4M14 9.5v3.6M14 9.5h-3.6"
        fill="none"
        stroke="currentColor"
        strokeLinecap="square"
        strokeWidth="1.25"
      />
    </svg>
  );
}

export function PlusIcon(props: IconProps) {
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true" focusable="false" {...props}>
      <path d="M8 2.5v11M2.5 8h11" fill="none" stroke="currentColor" strokeWidth="1.4" />
    </svg>
  );
}

export function LiveTrace({ active }: { active: boolean }) {
  return (
    <svg
      className={active ? "live-trace is-active" : "live-trace"}
      viewBox="0 0 48 12"
      aria-hidden="true"
      focusable="false"
    >
      <path
        className="live-trace-base"
        pathLength="100"
        d="M1 7h6l2-3 3 6 4-8 3 7 4-4 4 2 4-5 3 8 4-3 5 1h4"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.2"
        strokeLinejoin="miter"
        strokeLinecap="square"
      />
      <path
        className="live-trace-line"
        pathLength="100"
        d="M1 7h6l2-3 3 6 4-8 3 7 4-4 4 2 4-5 3 8 4-3 5 1h4"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.2"
        strokeLinejoin="miter"
        strokeLinecap="square"
      />
    </svg>
  );
}

export function BridgeMark(props: IconProps) {
  return (
    <svg viewBox="0 0 20 20" aria-hidden="true" focusable="false" {...props}>
      <path d="M3 4.5h14v11H3z" fill="none" stroke="currentColor" strokeWidth="1.2" />
      <path d="M5.5 8.5 8 11l-2.5 2.5M10 13.5h4.5" fill="none" stroke="currentColor" strokeWidth="1.2" />
    </svg>
  );
}
