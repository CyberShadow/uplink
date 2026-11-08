// Editing a draft in this tab (SPEC §7.2): the text as typed is saved, a
// field at a time, as splices against the server's revision. Edits from
// elsewhere (another tab, an undo) are rebased into the text as typed.

import { effect, signal } from '@preact/signals'
import type { Draft, DraftContent, DraftField } from '../generated/protocol'
import type { DraftStore } from '../stores/drafts'
import { FIELDS } from '../stores/drafts'
import { apply, diff, rebase, type Edit } from './splice'

/** How long typing pauses before it is saved. */
export const SAVE_DELAY_MS = 500

export type SessionState =
  /** Everything typed is saved. */
  | 'saved'
  | 'unsaved'
  /** The draft was sent or discarded, here or elsewhere. */
  | 'gone'

/** An edit of the text as typed, made by a rebase: `from` became `to`. */
export type Rebased = (field: DraftField, from: string, to: string) => void

interface InFlight {
  field: DraftField
  revision: number
  splice: Edit
}

export class ComposeSession {
  /** The text as typed, with edits from elsewhere rebased in. */
  readonly content
  readonly state = signal<SessionState>('saved')
  readonly error = signal<string | null>(null)
  readonly account: string
  readonly inReplyTo: Draft['in_reply_to']
  /** Called when a rebase changes the text as typed, so that editors can
   * update and keep the caret in place. */
  onRebase: Rebased | null = null

  /** The server's draft the text as typed is based on. */
  private base: Draft
  private inFlight: InFlight | null = null
  private timer: ReturnType<typeof setTimeout> | undefined
  private settled: (() => void)[] = []

  constructor(
    private readonly store: DraftStore,
    readonly id: string,
  ) {
    const draft = store.drafts.value?.get(id)
    if (!draft) throw new Error(`draft ${id} is not loaded`)
    this.base = draft
    this.account = draft.account
    this.inReplyTo = draft.in_reply_to
    this.content = signal<DraftContent>(draft.content)
    effect(() => {
      const drafts = this.store.drafts.value
      if (drafts) this.reconcile(drafts.get(id))
    })
  }

  /** The user changed `field` to `value`. */
  set(field: DraftField, value: string) {
    if (this.state.value === 'gone' || this.content.value[field] === value) return
    this.content.value = { ...this.content.value, [field]: value }
    this.state.value = 'unsaved'
    clearTimeout(this.timer)
    this.timer = setTimeout(() => this.save(), SAVE_DELAY_MS)
  }

  /** Saves what is typed now; resolves once all of it is saved. */
  flush(): Promise<void> {
    clearTimeout(this.timer)
    if (this.state.value !== 'unsaved') return Promise.resolve()
    const settled = new Promise<void>((resolve) => this.settled.push(resolve))
    this.save()
    return settled
  }

  /** The server's draft changed (or went). */
  private reconcile(server: Draft | undefined) {
    if (!server) {
      this.state.value = 'gone'
      clearTimeout(this.timer)
      this.settle()
      return
    }
    if (server.revision === this.base.revision) return
    const ours = this.inFlight
    if (ours && server.revision === ours.revision + 1 && this.isOurs(server, ours)) {
      // Our own edit: the text as typed has it already.
      this.inFlight = null
    } else {
      // Edits from elsewhere; ours, if in flight, will be refused.
      const content = { ...this.content.value }
      for (const field of FIELDS) {
        const from = content[field]
        const to = rebase(this.base.content[field], from, server.content[field])
        if (to === from) continue
        content[field] = to
        this.onRebase?.(field, from, to)
      }
      this.content.value = content
    }
    this.base = server
    this.update()
  }

  private isOurs(server: Draft, ours: InFlight): boolean {
    return FIELDS.every((field) =>
      field === ours.field
        ? server.content[field] === apply(this.base.content[field], ours.splice)
        : server.content[field] === this.base.content[field],
    )
  }

  /** Sends the next field's changes, unless an edit is in flight. */
  private async save() {
    if (this.inFlight || this.state.value === 'gone') return
    const field = FIELDS.find((f) => this.content.value[f] !== this.base.content[f])
    if (!field) return this.update()
    const splice = diff(this.base.content[field], this.content.value[field])!
    const ours = { field, revision: this.base.revision, splice }
    this.inFlight = ours
    let outcome
    try {
      outcome = await this.store.edit(this.id, ours.revision, { field, ...splice })
    } catch (error) {
      this.error.value = (error as Error).message
      // Unsaved; the next change tries again.
      if (this.inFlight === ours) this.inFlight = null
      return
    }
    switch (outcome) {
      case 'applied':
        // Its entry came first and settled it.
        if (this.inFlight === ours) throw new Error('edit applied without its entry')
        break
      case 'conflict':
        // Rebased already, by the entry that made it stale.
        if (this.inFlight === ours) this.inFlight = null
        break
      case 'gone':
        this.inFlight = null
        return
    }
    this.save()
  }

  private update() {
    if (this.state.value === 'gone') return
    const saved = !this.inFlight && FIELDS.every((f) => this.content.value[f] === this.base.content[f])
    this.state.value = saved ? 'saved' : 'unsaved'
    if (saved) this.settle()
  }

  private settle() {
    for (const resolve of this.settled.splice(0)) resolve()
  }
}
