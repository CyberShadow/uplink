import { render } from '@testing-library/preact'
import { describe, expect, it } from 'vitest'
import { Badge } from './Badge'

describe('Badge', () => {
  it('counts whole, past 99, and shows nothing of none', () => {
    const { container, rerender } = render(<Badge count={1234} tone="unread" title="Unread" />)
    expect(container.textContent).toBe((1234).toLocaleString())
    expect(container.textContent).not.toContain('+')
    rerender(<Badge count={0} tone="alert" title="Mentioning you" />)
    expect(container.textContent).toBe('')
  })
})
