// A subscription's window, as the client has it (SPEC §5.7): its items
// by key, kept current by what the server says changed of it.

import type { Item, ListScope, Listed, RoomId, UnknownNames, Unseen, WindowChange } from '../generated/protocol'
import type { Subscriber, Subscription } from '../connection'
import { before } from '../items'

/** What a window lists, by key, once `change` is applied to what it
 * listed, `was`: anew, not changed, so that `was` stays as it was. */
function applied<T>(was: ReadonlyMap<string, Listed<T>>, change: WindowChange<T>): Map<string, Listed<T>> {
  const listed = new Map(was)
  for (const key of change.removed) listed.delete(key)
  for (const one of change.upserted) listed.set(one.key, one)
  return listed
}

export interface Shown {
  /** By key. Replaced, not changed, as the window changes. */
  items: ReadonlyMap<string, Listed<Item>>
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
      change: (change) => this.take(this.shown!.items, change, this.shown!.scope),
      failed,
    })
  }

  /** Its items, the most recently active first. */
  listed(): Listed<Item>[] {
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

  private take(items: ReadonlyMap<string, Listed<Item>>, change: WindowChange<Item>, scope: ListScope) {
    const was = this.shown
    this.shown = { items: applied(items, change), more: change.more, scope, partialRooms: change.partial_rooms }
    this.listener?.(was)
  }
}

/** A window of the unseen (SPEC §5.7), as the page has it: across
 * connections, so that what a new one lists is told as what changed. */
export class UnseenWindow {
  /** By key; `null` until the server first says what it lists. */
  listed: ReadonlyMap<string, Listed<Unseen>> | null = null
  /** Told what it listed before (`null`, before it first listed) and
   * what it lists now, each time it changes. */
  listener:
    | ((before: ReadonlyMap<string, Listed<Unseen>> | null, now: ReadonlyMap<string, Listed<Unseen>>) => void)
    | null = null
  private readonly subscription: Subscription

  constructor(server: Subscriber, readonly query: string, limit: number, failed: (error: Error) => void) {
    this.subscription = server.subscribeUnseen(query, limit, {
      reset: (change) => this.take(new Map(), change),
      change: (change) => this.take(this.listed!, change),
      failed,
    })
  }

  close() {
    this.subscription.close()
  }

  private take(listed: ReadonlyMap<string, Listed<Unseen>>, change: WindowChange<Unseen>) {
    const was = this.listed
    this.listed = applied(listed, change)
    this.listener?.(was, this.listed)
  }
}
