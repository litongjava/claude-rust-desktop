import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/react';
import DeepSeekCodeLogo from '../components/DeepSeekCodeLogo';

describe('DeepSeekCodeLogo', () => {
  it('should render an svg element', () => {
    render(<DeepSeekCodeLogo />);
    const svg = document.querySelector('svg');
    expect(svg).toBeInTheDocument();
    expect(svg).toHaveAttribute('aria-hidden', 'true');
  });

  it('should accept className prop', () => {
    render(<DeepSeekCodeLogo className="test-class" />);
    const container = document.querySelector('div.test-class');
    expect(container).toBeInTheDocument();
  });

  it('should render with default props', () => {
    const { container } = render(<DeepSeekCodeLogo />);
    const mark = container.querySelector('svg > g > rect');
    expect(mark).toHaveAttribute('fill', 'currentColor');
  });

  it('should accept color prop', () => {
    render(<DeepSeekCodeLogo color="#FF0000" />);
    const svg = document.querySelector('svg');
    expect(svg).toHaveStyle({ color: '#FF0000' });
  });

  it('should apply style prop to the wrapper', () => {
    const { container } = render(<DeepSeekCodeLogo style={{ width: '24px', height: '24px' }} />);
    const wrapper = container.firstElementChild as HTMLElement;
    expect(wrapper).toHaveStyle({ width: '24px', height: '24px' });
  });

  it('should spin when autoAnimate is set', () => {
    render(<DeepSeekCodeLogo autoAnimate />);
    expect(document.querySelector('svg')).toHaveClass('deepseek-code-logo-spin');
  });

  it('should pulse when breathe is set', () => {
    render(<DeepSeekCodeLogo breathe />);
    expect(document.querySelector('svg')).toHaveClass('deepseek-code-logo-breathe');
  });

  it('should accept maxScale prop', () => {
    render(<DeepSeekCodeLogo maxScale={0.17} />);
    expect(document.querySelector('svg')).toBeInTheDocument();
  });

  it('should call onClick prop', () => {
    const onClick = vi.fn();
    const { container } = render(<DeepSeekCodeLogo onClick={onClick} />);
    const wrapper = container.firstElementChild as HTMLElement;
    fireEvent.click(wrapper);
    expect(onClick).toHaveBeenCalledTimes(1);
  });
});
