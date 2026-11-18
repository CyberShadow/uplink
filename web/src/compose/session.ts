// Editing a text in this tab (SPEC §7.2), of one field or several: a
// mail draft's, or what is written to a room (§7.6). The text as typed
// is saved, a field at a time, as splices against the server's revision.
// Edits from elsewhere (another tab, an undo) are rebased into the text
// as typed.

import { effect, signal, type Signal } from '@preact/signals'
import type { Splice } from '../generated/protocol'
import { apply, diff, rebase } from './splice'

/** How long typing pauses before it is saved. */
export const SAVE_DELAY_MS = 500

export type SessionState =
  /** Everything typed is saved. */
  | 'saved'
  | 'unsaved'
  /** The text is gone (a draft sent or discarded, here or elsewhere). */
  | 'gone'

/** The fields' texts. */
export type Content<F extends string> = Readonly<Record<F, string>>

/** A text as the server has it, at a revision. */
export interface Revised<F extends string> {
  readonly revision: number
  readonly content: Content<F>
}

/** What became of an edit: applied; refused, the text being at another
 * revision; or refused, the text being gone. */
export type EditOutcome = 'applied' | 'conflict' | 'gone'

/** The text a session edits, at the server. */
export interface Edited<F extends string> {
  /** Its fields, in the order saved. */
  readonly fields: readonly F[]
  /** As the server has it now (read reactively): `undefined` while that
   * is not known, `null` once it is gone. */
  server(): Revised<F> | null | undefined
  /** Applies `splice` to `field` of it at `revision`. */
  edit(revision: number, field: F, splice: Splice): Promise<EditOutcome>
}

/** An edit of the text as typed, made by a rebase: `from` became `to`. */
export type Rebased<F extends string> = (field: F, from: string, to: string) => void

interface InFlight<F extends string> {
  field: F
  revision: number
  splice: Splice
}

export class EditSession<F extends string> {
  /** The text as typed, with edits from elsewhere rebased in. */
  readonly content: Signal<Content<F>>
  readonly state = signal<SessionState>('saved')
  readonly error = signal<string | null>(null)
  /** Called when a rebase changes the text as typed, so that editors can
   * update and keep the caret in place. */
  onRebase: Rebased<F> | null = null

  /** The server's text the text as typed is based on. */
  private base: Revised<F>
  private inFlight: InFlight<F> | null = null
  private timer: ReturnType<typeof setTimeout> | undefined
  private settled: (() => void)[] = []

  /** Of `edited`, which the server's is known of. */
  constructor(private readonly edited: Edited<F>) {
    const server = edited.server()
    if (!server) throw new Error('the text to edit is not loaded')
    this.base = server
    this.content = signal(server.content)
    effect(() => {
      const server = this.edited.server()
      if (server !== undefined) this.reconcile(server)
    })
  }

  /** The user changed `field` to `value`. */
  set(field: F, value: string) {
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

  /** The server's text changed (or went). */
  private reconcile(server: Revised<F> | null) {
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
      const content: Record<F, string> = { ...this.content.value }
      for (const field of this.edited.fields) {
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

  private isOurs(server: Revised<F>, ours: InFlight<F>): boolean {
    return this.edited.fields.every((field) =>
      field === ours.field
        ? server.content[field] === apply(this.base.content[field], ours.splice)
        : server.content[field] === this.base.content[field],
    )
  }

  /** Sends the next field's changes, unless an edit is in flight. */
  private async save() {
    if (this.inFlight || this.state.value === 'gone') return
    const field = this.edited.fields.find((f) => this.content.value[f] !== this.base.content[f])
    if (!field) return this.update()
    const splice = diff(this.base.content[field], this.content.value[field])!
    const ours = { field, revision: this.base.revision, splice }
    this.inFlight = ours
    let outcome
    try {
      outcome = await this.edited.edit(ours.revision, field, splice)
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
    const saved = !this.inFlight && this.edited.fields.every((f) => this.content.value[f] === this.base.content[f])
    this.state.value = saved ? 'saved' : 'unsaved'
    if (saved) this.settle()
  }

  private settle() {
    for (const resolve of this.settled.splice(0)) resolve()
  }
}
