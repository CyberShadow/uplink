import { describe, expect, it } from 'vitest'
import type { Activity, Badge, Badges } from './generated/protocol'
import { iconOf, markOf } from './favicon'

const todos = (n: number): Activity[] => Array.from({ length: n }, (_, i) => ({ kind: 'todo', todo: i }))
const badges = (...buckets: [Badge, number][]): Badges => ({
  buckets: buckets.map(([badge, n], i) => ({ name: `B${i}`, badge, items: todos(n) })),
})

describe('the icon', () => {
  it("counts the count-badged buckets' items, to 99", () => {
    expect(markOf(badges(['count', 2], ['dot', 5], ['count', 1]))).toEqual({ kind: 'count', text: '3' })
    expect(markOf(badges(['count', 60], ['count', 40]))).toEqual({ kind: 'count', text: '99+' })
    expect(markOf(badges(['count', 99]))).toEqual({ kind: 'count', text: '99' })
  })

  it('has a dot while only dot-badged buckets hold anything, and else nothing', () => {
    expect(markOf(badges(['count', 0], ['dot', 1]))).toEqual({ kind: 'dot' })
    expect(markOf(badges(['count', 0], ['dot', 0]))).toBeNull()
    expect(markOf(null)).toBeNull()
  })

  it('is the logo, with the mark drawn over it', () => {
    expect(iconOf(null)).toBe('/logo.svg')
    const svg = decodeURIComponent(iconOf({ kind: 'count', text: '99+' }).replace(/^data:image\/svg\+xml,/, ''))
    expect(svg).toContain('<polygon points="16,0 29.86,8')
    expect(svg).toMatch(/<text [^>]*>99\+<\/text><\/svg>$/)
  })
})
