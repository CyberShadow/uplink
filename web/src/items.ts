// Items of every kind, as lists of them have them (SPEC §8.7): a search's
// and each home bucket's, the most recently active first.

import type { Item, ItemKind, ItemPosition } from './generated/protocol'
import { conversationKey } from './stores/mail'

/** An item as listed, by `query`. */
export type Row = Item & {
  /** Identifies it while it changes: a conversation's, as mail lists key
   * them. */
  key: string
  query: string
}

export function rowOf(item: Item, query: string): Row {
  return { ...item, key: keyOf(item), query }
}

export function keyOf(item: Item): string {
  switch (item.kind) {
    case 'mail':
      return conversationKey(item.conversation)
    case 'room':
      return `room:${item.room.id}`
    case 'todo':
      return `todo:${item.todo.id}`
    case 'event':
      return `event:${item.event.id}`
  }
}

const KINDS: readonly ItemKind[] = ['mail', 'room', 'todo', 'event']

/** Where it is listed, as the server orders lists of every kind. */
export function positionOf(item: Item): ItemPosition {
  switch (item.kind) {
    case 'mail':
      return { at: item.conversation.latest.received_at, kind: 'mail', id: item.conversation.latest.id }
    case 'room':
      // An invitation has no activity of its own: the oldest.
      return { at: item.hits?.latest.ts ?? item.room.last_event_ts ?? 0, kind: 'room', id: item.room.id }
    case 'todo':
      return { at: item.todo.updated_at, kind: 'todo', id: item.todo.id }
    case 'event':
      return { at: item.event.modified, kind: 'event', id: item.event.id }
  }
}

/** Of two positions, which is listed first: negative if `p`. */
export function before(p: ItemPosition, q: ItemPosition): number {
  return q.at - p.at || KINDS.indexOf(p.kind) - KINDS.indexOf(q.kind) || q.id - p.id
}

/** Of two items, which is listed first: negative if `a`. */
export function compare(a: Item, b: Item): number {
  return before(positionOf(a), positionOf(b))
}
