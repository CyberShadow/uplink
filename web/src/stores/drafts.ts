// Drafts and the outbox, as the server has them (SPEC §6.6, §6.7, §7), and
// the compose sessions editing drafts in this tab.

import { signal } from '@preact/signals'
import type {
  AccountOutbox,
  ClientRequest,
  Draft,
  DraftContent,
  DraftAttachmentId,
  DraftEdited,
  DraftField,
  LogEntry,
  MessageContent,
  MessageId,
  OutboxId,
  ServerResponse,
  Splice,
  TxLogId,
} from '../generated/protocol'
import type { Blobs } from '../blobs'
import { RequestError, type Requester } from '../connection'
import { ComposeSession } from '../compose/session'
import { apply } from '../compose/splice'
import { Loader, query, type Response } from './query'
import type { Ops } from './ops'

export const FIELDS: readonly DraftField[] = ['to', 'cc', 'bcc', 'subject', 'body']

export type EditOutcome = 'applied' | 'conflict' | 'gone'

export class DraftStore {
  /** By id; `null` until loaded. */
  readonly drafts = signal<ReadonlyMap<string, Draft> | null>(null)
  /** Per account with anything, what is not yet at its provider. */
  readonly outbox = signal<readonly AccountOutbox[] | null>(null)
  readonly error = signal<string | null>(null)

  private sessions = new Map<string, ComposeSession>()
  private draftLoader = new Loader(async () => {
    const response = await this.query({ type: 'list_drafts' }, 'drafts')
    if (response) this.drafts.value = new Map(response.drafts.map((d) => [d.id, d]))
  })
  private outboxLoader = new Loader(async () => {
    const response = await this.query({ type: 'list_outbox' }, 'outbox')
    if (response) this.outbox.value = response.accounts
  })

  constructor(
    private readonly server: Requester & Blobs,
    private readonly ops: Ops,
  ) {}

  reset() {
    this.draftLoader.invalidate()
    this.outboxLoader.invalidate()
  }

  apply(entry: LogEntry) {
    for (const event of entry.events) {
      switch (event.type) {
        case 'draft_edited':
          this.applyEdit(event)
          break
        case 'drafts_changed':
          this.draftLoader.invalidate()
          break
        case 'outbox_changed':
          this.outboxLoader.invalidate()
          break
        case 'entries_added':
        case 'entries_removed':
        case 'labels_changed':
        case 'label_list_changed':
        case 'rooms_changed':
        case 'timeline_added':
        case 'matrix_sends_changed':
        case 'todos_changed':
        case 'calendar_changed':
          break
      }
    }
  }

  private applyEdit(event: DraftEdited) {
    const drafts = this.drafts.value
    // Not loaded yet: the load in flight covers the edit.
    if (!drafts) return
    const draft = drafts.get(event.draft)
    if (!draft || event.revision !== draft.revision + 1) {
      throw new Error(`draft ${event.draft} edited at revision ${event.revision}, but known at ${draft?.revision}`)
    }
    const field = event.splice.field
    const content = { ...draft.content, [field]: apply(draft.content[field], event.splice) }
    this.drafts.value = new Map(drafts).set(draft.id, {
      ...draft,
      revision: event.revision,
      content,
    })
  }

  /** The session editing draft `id` in this tab; the draft must be loaded. */
  session(id: string): ComposeSession {
    let session = this.sessions.get(id)
    if (!session) {
      session = new ComposeSession(this, id)
      this.sessions.set(id, session)
    }
    return session
  }

  // ── ops ─────────────────────────────────────────────────────────────────

  /** Creates a draft; resolves to its id, or `null` if that failed. */
  async create(account: string, content: DraftContent, inReplyTo: MessageId | null): Promise<string | null> {
    const id = crypto.randomUUID()
    const entry = await this.ops.dispatch(
      { type: 'create_draft', id, account, content, in_reply_to: inReplyTo },
      true,
    )
    return entry && id
  }

  /** Not through `ops`: edits are saved as typed, without notices, and a
   * conflict is expected now and then. */
  async edit(draft: string, revision: number, splice: Splice): Promise<EditOutcome> {
    try {
      await this.server.request({ type: 'edit_draft', draft, revision, splice })
      return 'applied'
    } catch (error) {
      if (error instanceof RequestError && error.error.code === 'conflict') {
        return this.drafts.value?.has(draft) ? 'conflict' : 'gone'
      }
      throw error
    }
  }

  /** Uploads `files` and attaches them; resolves once all are, or one
   * failed. */
  async attach(draft: string, files: readonly File[]): Promise<void> {
    for (const file of files) {
      let uploaded
      try {
        uploaded = await this.server.upload(file)
      } catch (error) {
        this.ops.fail(`Could not attach ${file.name}: ${(error as Error).message}`)
        return
      }
      const entry = await this.ops.dispatch({
        type: 'attach_to_draft',
        draft,
        blob: uploaded.blob,
        name: file.name,
        mime: file.type || 'application/octet-stream',
      })
      if (!entry) return
    }
  }

  /** Attaches the files `message` has, as forwarding it does; not the
   * images its body shows, which the forward's text does not. */
  async attachForwarded(draft: string, message: MessageContent): Promise<void> {
    for (const a of message.attachments.filter((a) => !a.inline)) {
      let copied
      try {
        copied = await this.server.copyAttachment(message.id, a.index)
      } catch (error) {
        this.ops.fail(`Could not attach ${a.name ?? 'an attachment'}: ${(error as Error).message}`)
        return
      }
      const entry = await this.ops.dispatch(
        {
          type: 'attach_to_draft',
          draft,
          blob: copied.blob,
          name: a.name ?? `attachment-${a.index}`,
          mime: a.mime,
        },
        true,
      )
      if (!entry) return
    }
  }

  detach(draft: string, attachment: DraftAttachmentId) {
    return this.ops.dispatch({ type: 'detach_from_draft', draft, attachment })
  }

  discard(draft: string) {
    return this.ops.dispatch({ type: 'delete_draft', draft })
  }

  send(draft: string) {
    return this.ops.dispatch({ type: 'send_draft', draft })
  }

  cancelSend(item: OutboxId) {
    return this.ops.dispatch({ type: 'cancel_send', item })
  }

  /** Resolves a send that went out though cancelled by keeping it sent. */
  keepSent(item: OutboxId) {
    return this.ops.dispatch({ type: 'keep_sent', item })
  }

  /** Resolves `op`'s refused changes in `account` by keeping what the
   * provider has. */
  keepObserved(account: string, op: TxLogId | null) {
    return this.ops.dispatch({ type: 'keep_observed', account, op })
  }

  private async query<T extends ServerResponse['type']>(
    request: ClientRequest,
    type: T,
  ): Promise<Response<T> | null> {
    try {
      return await query(this.server, request, type)
    } catch (error) {
      this.error.value = (error as Error).message
      return null
    }
  }
}

/** How many things the outbox lists: each op's changes, and each send. */
export function outboxCount(outbox: readonly AccountOutbox[]): number {
  return outbox.reduce((n, { pending }) => {
    switch (pending.kind) {
      case 'mail':
        return n + pending.changes.length + pending.sends.length
      case 'matrix':
        return n + pending.sends.length
    }
  }, 0)
}

/** Whether something in the outbox needs the user: an account failing,
 * something refused, or mail sent though cancelled. */
export function outboxNeedsAttention(outbox: readonly AccountOutbox[]): boolean {
  const decision = (s: { state: string; wanted: boolean }) => s.state === 'refused' || (s.state === 'sent' && !s.wanted)
  return outbox.some(({ failing, pending }) => {
    if (failing !== null) return true
    switch (pending.kind) {
      case 'mail':
        return pending.changes.some((c) => c.refused.length > 0) || pending.sends.some(decision)
      case 'matrix':
        return pending.sends.some(decision)
    }
  })
}
