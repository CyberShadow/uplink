// Where the user puts each top-level space (MSC3230, SPEC §9.5): its
// `order`, a string of `\x20`–`\x7E` at most 50 long, compared character
// by character, as Element keeps it in the space's room account data.

import type { Room, SpaceOrderChange } from './generated/protocol'

const MAX_LENGTH = 50
/** The orders' alphabet: `\x20`–`\x7E`. */
const FIRST = 0x20
const BASE = 95n

/** Top-level spaces as MSC3230 orders them: by order, those without
 * last; then by room id. */
export function bySpaceOrder(a: Room, b: Room): number {
  return compare(a.space_order, b.space_order) || compare(a.room_id, b.room_id)
}

/** Strings by their characters; `null` after any. */
function compare(a: string | null, b: string | null): number {
  if (a === b) return 0
  if (a === null) return 1
  if (b === null) return -1
  return a < b ? -1 : 1
}

/** The changes that put `moved`, of `spaces` (as `bySpaceOrder` orders
 * them), before `before`, or last: as MSC3230 recommends, as few as can
 * be. The moved space takes an order between its new neighbours'; those
 * before it without one take one too, as they must stay before it. Where
 * no order fits between, the neighbours after move as well, then those
 * before. None if it is where it was. */
export function moveSpace(spaces: readonly Room[], moved: Room, before: Room | null): SpaceOrderChange[] {
  const rest = spaces.filter((space) => space !== moved)
  const at = before === null ? rest.length : rest.indexOf(before)
  if (at === -1) throw new Error(`${before!.room_id} is not among the spaces`)
  const list = [...rest.slice(0, at), moved, ...rest.slice(at)]
  if (list.every((space, i) => space === spaces[i])) return []
  const order = (i: number) => (i === at ? null : list[i]!.space_order)
  // Those before without an order (the last of those before) take one.
  let first = at
  while (first > 0 && order(first - 1) === null) first--
  let last = at
  for (;;) {
    const below = first > 0 ? order(first - 1)! : ''
    const above = last + 1 < list.length ? order(last + 1) : null
    const orders = between(below, above, last - first + 1)
    if (orders) {
      return orders.map((to, i) => ({ space: list[first + i]!.id, from: list[first + i]!.space_order, to }))
    }
    if (above !== null) last++
    else if (first > 0) first--
    // Between nothing and nothing, any number fit.
    else throw new Error('no orders fit')
  }
}

/** `count` orders strictly between `below` and `above` (`null`: no
 * bound), evenly spaced, as short as can be: as numbers in base 95, the
 * shorter padded with the first character, as MSC3230 suggests; `null`
 * if they do not fit. */
export function between(below: string, above: string | null, count: number): string[] | null {
  for (let length = Math.max(below.length, above?.length ?? 0, 1); length <= MAX_LENGTH; length++) {
    const low = value(below, length)
    const high = above === null ? BASE ** BigInt(length) : value(above, length)
    if (high - low - 1n < BigInt(count)) continue
    const step = (high - low) / BigInt(count + 1)
    return Array.from({ length: count }, (_, i) => string(low + step * BigInt(i + 1), length))
  }
  return null
}

function value(order: string, length: number): bigint {
  let value = 0n
  for (let i = 0; i < length; i++) value = value * BASE + BigInt(i < order.length ? order.charCodeAt(i) - FIRST : 0)
  return value
}

function string(value: bigint, length: number): string {
  const chars: string[] = []
  for (let i = 0; i < length; i++) {
    chars.unshift(String.fromCharCode(FIRST + Number(value % BASE)))
    value /= BASE
  }
  return chars.join('')
}
