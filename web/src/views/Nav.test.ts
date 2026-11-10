import { describe, expect, it } from 'vitest'
import type { Label } from '../generated/protocol'
import { labelTree, record, remaining, type Sample } from './Nav'

describe('a first sync', () => {
  const minutes = (n: number) => n * 60_000

  it('has a pace once its count has risen', () => {
    let seen: Sample[] = record([], { at: 0, stored: 100 })
    expect(remaining(seen, 1000)).toBeNull()
    seen = record(seen, { at: minutes(1), stored: 100 })
    expect(seen).toHaveLength(1)
    seen = record(seen, { at: minutes(2), stored: 260 })
    // 80 a minute, 740 to go.
    expect(remaining(seen, 1000)).toBe(minutes(740 / 80))
  })

  it('is paced by the last ten minutes', () => {
    let seen: Sample[] = []
    for (let m = 0; m <= 30; m++) seen = record(seen, { at: minutes(m), stored: m < 20 ? m * 10 : 190 + (m - 19) * 100 })
    // The window, and the one sample before it.
    expect(seen[0].at).toBe(minutes(19))
    expect(remaining(seen, 2290)).toBe(minutes(10))
  })
})

describe('labelTree', () => {
  const label = (id: number, name: string): Label => ({
    id,
    external_id: `Label_${id}`,
    name,
    system: false,
    folder: false,
    total: 0,
    unread: 0,
  })

  it('nests labels by their slashes, under parents that may not be labels', () => {
    const tree = labelTree([label(1, 'Work/Uplink'), label(2, 'Work'), label(3, 'Travel/2026'), label(4, 'Alpha')])
    const shape = (nodes: ReturnType<typeof labelTree>): unknown =>
      nodes.map((n) => [n.name, n.label?.id ?? null, shape(n.children)])
    expect(shape(tree)).toEqual([
      ['Alpha', 4, []],
      ['Travel', null, [['2026', 3, []]]],
      ['Work', 2, [['Uplink', 1, []]]],
    ])
  })
})
