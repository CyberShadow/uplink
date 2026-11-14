import { describe, expect, it } from 'vitest'
import type { Badges } from './generated/protocol'
import { iconOf, markOf } from './favicon'

const NONE = { count: 0, dot: false }
const badges = (count: number, dot: boolean): Badges => ({
  marks: { home: { count, dot }, labels: [], accounts: [], rooms: [], todos: NONE, calendar: NONE },
})

describe('the icon', () => {
  it("counts the count-badged buckets' items, to 99", () => {
    expect(markOf(badges(3, true))).toEqual({ kind: 'count', text: '3' })
    expect(markOf(badges(100, false))).toEqual({ kind: 'count', text: '99+' })
    expect(markOf(badges(99, false))).toEqual({ kind: 'count', text: '99' })
  })

  it('has a dot while only dot-badged buckets hold anything, and else nothing', () => {
    expect(markOf(badges(0, true))).toEqual({ kind: 'dot' })
    expect(markOf(badges(0, false))).toBeNull()
    expect(markOf(null)).toBeNull()
  })

  it('is the logo, with the mark drawn over it', () => {
    expect(iconOf(null)).toBe('/logo.svg')
    const svg = decodeURIComponent(iconOf({ kind: 'count', text: '99+' }).replace(/^data:image\/svg\+xml,/, ''))
    expect(svg).toContain('<polygon points="16,0 29.86,8')
    expect(svg).toMatch(/<text [^>]*>99\+<\/text><\/svg>$/)
  })
})
