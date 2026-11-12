import { describe, expect, it } from 'vitest'
import type { Label, MatrixAccount, Room } from '../generated/protocol'
import { foldsOver, labelTree, record, remaining, roomTree, type RoomNode, type Sample } from './Nav'
import { avatarPath, initial } from './RoomAvatar'

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

describe('roomTree', () => {
  let next = 1
  const room = (name: string, fields: Partial<Room> = {}): Room => ({
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
    space: false,
    direct: false,
    latest: null,
    avatar: null,
    children: [],
    topic: null,
    alias: null,
    joined_members: 2,
    notifying: 0,
    highlighted: 0,
    pinned: [],
    may_pin: false,
    space_order: null,
    tags: [],
    ...fields,
  })
  const space = (name: string, children: Room[], fields: Partial<Room> = {}) =>
    room(name, { space: true, children: children.map((c) => c.room_id), ...fields })
  const names = (nodes: RoomNode[]): unknown[] =>
    nodes.map((n) => (n.room.space && n.room.membership === 'joined' ? { [n.room.name]: names(n.children) } : n.room.name))

  it('nests rooms in spaces: subspaces in order, then rooms by activity, then the rest', () => {
    const [recent, old, loose] = [room('recent'), room('old'), room('loose')]
    const sub = space('sub', [old])
    const top = space('top', [old, recent, sub])
    // By activity, as listed.
    const rooms = [recent, loose, top, old, sub]
    expect(names(roomTree(rooms))).toEqual([{ top: [{ sub: ['old'] }, 'recent', 'old'] }, 'loose'])
  })

  it('finds the folds over each place a room is in', () => {
    const [old, loose] = [room('old'), room('loose')]
    const sub = space('sub', [old])
    const top = space('top', [sub, old])
    const rooms = [loose, top, old, sub]
    const accounts = [{ id: 'mx', mxid: '@me:x', synced: true } as MatrixAccount]
    expect(foldsOver(`/rooms/${old.id}`, [], accounts, rooms)).toEqual([
      ['matrix:mx', 'matrix:mx:!top:x', 'matrix:mx:!sub:x'],
      ['matrix:mx', 'matrix:mx:!top:x'],
    ])
    expect(foldsOver(`/rooms/${loose.id}`, [], accounts, rooms)).toEqual([['matrix:mx']])
    // A joined space is not a link.
    expect(foldsOver(`/rooms/${top.id}`, [], accounts, rooms)).toEqual([])
    expect(foldsOver('/drafts', [], accounts, rooms)).toEqual([[]])
  })

  it('puts top-level spaces in the order given them, and leaves out children not listed', () => {
    // Those without an order last, by room id.
    const c = space('c', [])
    const b = space('b', [], { space_order: 'b' })
    const a = space('a', [room('unjoined')])
    const d = space('d', [], { space_order: 'a' })
    expect(names(roomTree([c, b, a, d]))).toEqual([{ d: [] }, { b: [] }, { a: [] }, { c: [] }])
  })

  it('does not nest invited spaces, nor in them', () => {
    const child = room('child')
    const invited = space('invited', [child], { membership: 'invited' })
    expect(names(roomTree([invited, child]))).toEqual(['invited', 'child'])
  })

  it('puts spaces only in each other at the top, once', () => {
    const a = room('a', { space: true })
    const b = space('b', [a])
    a.children = [b.room_id]
    expect(names(roomTree([a, b]))).toEqual([{ a: [{ b: [] }] }])
  })
})

describe('room avatars', () => {
  it('are served at a path naming them', () => {
    const room = { id: 7, avatar: 'mxc://example.org:8448/AbC_d-1' } as Room
    expect(avatarPath(room)).toBe('/api/rooms/7/avatar/example.org%3A8448/AbC_d-1')
    expect(avatarPath({ ...room, avatar: null })).toBeNull()
    expect(avatarPath({ ...room, avatar: 'https://example.org/a' })).toBeNull()
  })

  it('fall back to the first letter of the name', () => {
    expect(initial('#matrix-dev')).toBe('M')
    expect(initial('élan')).toBe('É')
    expect(initial('!!!')).toBe('?')
  })
})
