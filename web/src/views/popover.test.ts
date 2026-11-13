import { describe, expect, it } from 'vitest'
import { opensAbove } from './popover'

describe('opensAbove', () => {
  const clip = { top: 0, bottom: 600 }
  it('opens below where it fits', () => {
    expect(opensAbove({ top: 100, bottom: 120 }, 324, clip)).toBe(false)
  })
  it('opens above where it would be cut off below, and there is more room above', () => {
    expect(opensAbove({ top: 500, bottom: 520 }, 724, clip)).toBe(true)
  })
  it('opens below, cut off, where there is less room above', () => {
    expect(opensAbove({ top: 50, bottom: 70 }, 774, clip)).toBe(false)
  })
})
