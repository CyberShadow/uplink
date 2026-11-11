import { describe, expect, it } from 'vitest'
import type { Room, SpaceOrderChange } from './generated/protocol'
import { between, bySpaceOrder, moveSpace } from './spaceOrder'

let next = 1
const space = (name: string, space_order: string | null = null): Room => ({
  id: next++,
  account: 'mx',
  room_id: `!${name}:x`,
  name,
  encrypted: false,
  membership: 'joined',
  unread: 0,
  last_event_ts: null,
  read_up_to: null,
  pending: null,
  membership_error: null,
  space: true,
  direct: false,
  avatar: null,
  children: [],
  topic: null,
  joined_members: 1,
  notifying: 0,
  highlighted: 0,
  pinned: [],
  may_pin: false,
  space_order,
  tags: [],
})

/** The spaces with `changes` made, as they then go. */
function applied(spaces: readonly Room[], changes: SpaceOrderChange[]): Room[] {
  return spaces
    .map((s) => {
      const change = changes.find((c) => c.space === s.id)
      if (!change) return s
      expect(change.from).toBe(s.space_order)
      return { ...s, space_order: change.to }
    })
    .sort(bySpaceOrder)
}

const names = (spaces: readonly Room[]) => spaces.map((s) => s.name)

describe('bySpaceOrder', () => {
  it('puts spaces by order, those without last, then by room id', () => {
    const spaces = [space('d'), space('c', 'b'), space('b'), space('a', 'b'), space('e', 'a')]
    expect(names([...spaces].sort(bySpaceOrder))).toEqual(['e', 'a', 'c', 'b', 'd'])
  })
})

describe('moveSpace', () => {
  it('orders only the space moved between ordered neighbours', () => {
    const [a, b, c] = [space('a', 'a'), space('b', 'b'), space('c', 'c')]
    const changes = moveSpace([a, b, c], c, b)
    expect(changes.map((ch) => ch.space)).toEqual([c.id])
    expect(names(applied([a, b, c], changes))).toEqual(['a', 'c', 'b'])
  })

  it('orders those before without one, but not those after', () => {
    const [a, b, c, d] = [space('a'), space('b'), space('c'), space('d')]
    const changes = moveSpace([a, b, c, d], d, c)
    expect(changes.map((ch) => ch.space)).toEqual([a.id, b.id, d.id])
    expect(names(applied([a, b, c, d], changes))).toEqual(['a', 'b', 'd', 'c'])
    // To the top: only the space moved.
    expect(moveSpace([a, b, c, d], c, a).map((ch) => ch.space)).toEqual([c.id])
  })

  it('moves neighbours where no order fits between', () => {
    const [a, b, c] = [space('a', 'm'), space('b', 'm'), space('c', 'n')]
    const changes = moveSpace([a, b, c], c, b)
    expect(names(applied([a, b, c], changes))).toEqual(['a', 'c', 'b'])
    expect(changes.map((ch) => ch.space)).toEqual([c.id, b.id])
  })

  it('changes nothing in place', () => {
    const [a, b] = [space('a'), space('b')]
    expect(moveSpace([a, b], a, b)).toEqual([])
    expect(moveSpace([a, b], b, null)).toEqual([])
  })

  it('puts any space anywhere', () => {
    // Orders crowded together, with ties, and none.
    const alphabet = [' ', '!', 'a', '~']
    let seed = 1
    const random = (n: number) => {
      seed = (seed * 48271) % 0x7fffffff
      return seed % n
    }
    for (let round = 0; round < 500; round++) {
      const spaces = Array.from({ length: 1 + random(6) }, (_, i) => {
        const order = random(4) === 0 ? null : Array.from({ length: random(3) }, () => alphabet[random(4)]).join('')
        return space(`s${i}`, order)
      }).sort(bySpaceOrder)
      const moved = spaces[random(spaces.length)]!
      const rest = spaces.filter((s) => s !== moved)
      const at = random(rest.length + 1)
      const wanted = [...rest.slice(0, at), moved, ...rest.slice(at)]
      const changes = moveSpace(spaces, moved, rest[at] ?? null)
      expect(names(applied(spaces, changes)), JSON.stringify(spaces)).toEqual(names(wanted))
      for (const change of changes) expect(change.to!.length).toBeLessThanOrEqual(50)
    }
  })
})

describe('between', () => {
  it('spaces orders out, short', () => {
    expect(between('', null, 2)).toEqual(['?', '^'])
    expect(between('a', 'b', 1)).toEqual(['aO'])
  })

  it('fits none past the longest order', () => {
    expect(between('~'.repeat(50), null, 1)).toBeNull()
    expect(between('a', 'a ', 1)).toBeNull()
  })
})
