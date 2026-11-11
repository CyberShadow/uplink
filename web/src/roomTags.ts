// A Matrix room's tags (SPEC §9.5): Element's two, and the user's own.

import type { Room } from './generated/protocol'

export const FAVOURITE = 'm.favourite'
export const LOW_PRIORITY = 'm.lowpriority'

/** The tag as the user knows it: Element's name of its own, a user tag's
 * name, or else as it is. */
export function tagName(tag: string): string {
  if (tag === FAVOURITE) return 'Favourite'
  if (tag === LOW_PRIORITY) return 'Low priority'
  return tag.startsWith('u.') ? tag.slice(2) : tag
}

/** The tag a name typed makes: one of the user's own. */
export function userTag(name: string): string {
  return `u.${name.trim()}`
}

/** The tags to offer for `rooms`: Element's two, the user's own of any
 * room of the account, and any other the rooms have, as named. */
export function knownTags(all: readonly Room[], rooms: readonly Room[]): string[] {
  const accounts = new Set(rooms.map((r) => r.account))
  const own = all.filter((r) => accounts.has(r.account)).flatMap((r) => r.tags.filter((t) => t.startsWith('u.')))
  const others = rooms.flatMap((r) => r.tags)
  const rest = [...new Set([...own, ...others])].filter((t) => t !== FAVOURITE && t !== LOW_PRIORITY)
  return [FAVOURITE, LOW_PRIORITY, ...rest.sort((a, b) => tagName(a).localeCompare(tagName(b)))]
}
