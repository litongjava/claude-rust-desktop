import React, { useEffect, useId, useRef, useState } from 'react';
import './DeepSeekCodeLogo.css';

interface DeepSeekCodeLogoProps {
  className?: string;
  style?: React.CSSProperties;
  onClick?: () => void;
  autoAnimate?: boolean; // 对应 thinking 模式
  breathe?: boolean;     // 对应 waiting 模式
  color?: string;
  maxScale?: number;     // cap the drawing scale (e.g. 0.17 to keep logo small in a large canvas)
}

// Eight tapered rays around an octagonal core. The icon's `>_` prompt is cut
// out of the core with a luminance mask so the background stays transparent.
const RAY = 'M -6.8 -6 L -5.6 -37.5 L -4.7 -41.5 L 4.7 -41.5 L 5.6 -37.5 L 6.8 -6 Z';
const CORE =
  'M 72.63 59.38 L 59.38 72.63 L 40.62 72.63 L 27.37 59.38 L 27.37 40.62 L 40.62 27.37 L 59.38 27.37 L 72.63 40.62 Z';
const RAY_ANGLES = [0, 45, 90, 135, 180, 225, 270, 315];

const DeepSeekCodeLogo: React.FC<DeepSeekCodeLogoProps> = ({
  className = '',
  style,
  onClick,
  autoAnimate = false,
  breathe = false,
  color = '#D97757',
  maxScale,
}) => {
  const containerRef = useRef<HTMLDivElement>(null);
  const [contentScale, setContentScale] = useState(1);
  const maskId = `deepseek-code-logo-${useId().replace(/[^a-zA-Z0-9_-]/g, '')}`;

  useEffect(() => {
    if (maxScale === undefined) {
      setContentScale(1);
      return;
    }
    const container = containerRef.current;
    if (!container) return;

    const update = () => {
      const minDim = Math.min(container.clientWidth, container.clientHeight);
      if (!minDim) {
        setContentScale(1);
        return;
      }
      // Match the previous canvas behaviour: the mark spans 85% of the
      // smaller side unless maxScale caps the 240-unit reference drawing.
      const natural = (minDim / 240) * 0.85;
      setContentScale(Math.min(1, maxScale / natural));
    };

    update();
    const resizeObserver = typeof ResizeObserver !== 'undefined'
      ? new ResizeObserver(update)
      : null;
    resizeObserver?.observe(container);
    window.addEventListener('resize', update);
    return () => {
      resizeObserver?.disconnect();
      window.removeEventListener('resize', update);
    };
  }, [maxScale]);

  const motionClass = breathe
    ? 'deepseek-code-logo-breathe'
    : autoAnimate
      ? 'deepseek-code-logo-spin'
      : '';
  const contentTransform = contentScale === 1
    ? undefined
    : `translate(${50 - 50 * contentScale} ${50 - 50 * contentScale}) scale(${contentScale})`;

  return (
    <div
      ref={containerRef}
      className={`deepseek-code-logo ${className}`.trim()}
      style={{ width: '100%', height: '100%', ...style }}
      onClick={onClick}
    >
      <svg
        viewBox="0 0 100 100"
        width="100%"
        height="100%"
        aria-hidden="true"
        focusable="false"
        className={motionClass}
        style={{ display: 'block', color }}
      >
        <defs>
          <mask id={maskId} maskUnits="userSpaceOnUse" x="0" y="0" width="100" height="100">
            <g fill="#fff">
              {RAY_ANGLES.map((angle) => (
                <path
                  key={angle}
                  d={RAY}
                  transform={`translate(50 50) rotate(${angle})`}
                  stroke="#fff"
                  strokeWidth="1"
                  strokeLinejoin="round"
                />
              ))}
              <path d={CORE} stroke="#fff" strokeWidth="1.5" strokeLinejoin="round" />
            </g>
            <path
              d="M 38.2 40 L 50 47.8 L 38.7 56.6"
              fill="none"
              stroke="#000"
              strokeWidth="7.4"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
            <rect x="49.2" y="51.3" width="13.8" height="4.6" rx="2.3" fill="#000" />
          </mask>
        </defs>
        <g transform={contentTransform}>
          <rect width="100" height="100" fill="currentColor" mask={`url(#${maskId})`} />
        </g>
      </svg>
    </div>
  );
};

export default DeepSeekCodeLogo;
