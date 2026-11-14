import { describe, expect, it } from 'vitest'
import type { Badges, Mark } from './generated/protocol'
import { iconOf, markOf } from './favicon'

const NONE = { alert: 0, count: 0, dot: false }
const badges = (home: Partial<Mark>): Badges => ({
  marks: { home: { ...NONE, ...home }, labels: [], accounts: [], rooms: [], todos: NONE, calendar: NONE },
})

describe('the icon', () => {
  it("counts the alert-badged buckets' items in red, to 99", () => {
    expect(markOf(badges({ alert: 3, count: 2, dot: true }))).toEqual({ kind: 'alert', text: '3' })
    expect(markOf(badges({ alert: 100 }))).toEqual({ kind: 'alert', text: '99+' })
    expect(markOf(badges({ alert: 99 }))).toEqual({ kind: 'alert', text: '99' })
  })

  it("else the count-badged buckets' items", () => {
    expect(markOf(badges({ count: 2, dot: true }))).toEqual({ kind: 'count', text: '2' })
  })

  it('has a dot while only dot-badged buckets hold anything, and else nothing', () => {
    expect(markOf(badges({ dot: true }))).toEqual({ kind: 'dot' })
    expect(markOf(badges({}))).toBeNull()
    expect(markOf(null)).toBeNull()
  })

  it('is the logo, with the mark drawn over it', () => {
    expect(iconOf(null)).toBe('/logo.svg')
    const svg = (mark: Parameters<typeof iconOf>[0]) =>
      decodeURIComponent(iconOf(mark).replace(/^data:image\/svg\+xml,/, ''))
    const alert = svg({ kind: 'alert', text: '99+' })
    expect(alert).toContain('<polygon points="16,0 29.86,8')
    expect(alert).toMatch(/<text [^>]*>99\+<\/text><\/svg>$/)
    expect(alert).toContain('fill="#d93025"')
    expect(svg({ kind: 'count', text: '2' })).toContain('fill="#1a73e8"')
  })
})
