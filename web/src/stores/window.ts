// A subscription's window, as the client has it (SPEC §5.7): its items
// by key, kept current by what the server says changed of it.

import type { ListScope, Listed, RoomId, UnknownNames, WindowChange } from '../generated/protocol'
import type { Subscriber, Subscription } from '../connection'
import { before } from '../items'

export interface Shown {
  /** By key. Replaced, not changed, as the window changes. */
  items: ReadonlyMap<string, Listed>
  more: boolean
  scope: ListScope
  partialRooms: RoomId[]
}

export class LiveWindow {
  /** `null` until the server first says what it lists. */
  shown: Shown | null = null
  /** Told what it listed before, each time it changes; `null` while no
   * one shows it. */
  listener: ((before: Shown | null) => void) | null = null
  /** How many items it was asked for, of pages of `page`. */
  private asked: number
  private readonly subscription: Subscription

  constructor(
    server: Subscriber,
    readonly query: string,
    unknown: UnknownNames,
    private readonly page: number,
    failed: (error: Error) => void,
  ) {
    this.asked = page
    this.subscription = server.subscribe(query, unknown, page, {
      reset: (change, scope) => {
        // Subscribed to anew from its first page.
        this.asked = page
        this.take(new Map(), change, scope)
      },
      change: (change) => this.take(new Map(this.shown!.items), change, this.shown!.scope),
      failed,
    })
  }

  /** Its items, the most recently active first. */
  listed(): Listed[] {
    return [...this.shown!.items.values()].sort((a, b) => before(a.position, b.position))
  }

  /** Widens it by a page, unless it lists all there is, or is widening
   * already: while it lists more, it has as many as asked for once its
   * widening is in. */
  extend() {
    const shown = this.shown
    if (!shown?.more || shown.items.size < this.asked) return
    this.asked += this.page
    this.subscription.extend(this.page)
  }

  close() {
    this.subscription.close()
  }

  private take(items: Map<string, Listed>, change: WindowChange, scope: ListScope) {
    for (const key of change.removed) items.delete(key)
    for (const listed of change.upserted) items.set(listed.key, listed)
    const was = this.shown
    this.shown = { items, more: change.more, scope, partialRooms: change.partial_rooms }
    this.listener?.(was)
  }
}
