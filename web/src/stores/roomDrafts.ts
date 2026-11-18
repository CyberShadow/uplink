// What is being written to rooms, as the server has it (SPEC §7.6), and
// the sessions editing it in this tab: of each box (a room's, or one of
// its threads'), the text as typed, what it replies to, and the files to
// send with it.

import { computed, signal } from '@preact/signals'
import type {
  DraftAttachmentId,
  LogEntry,
  RoomBox,
  RoomDraft,
  RoomDraftEdited,
  RoomEvent,
  RoomId,
  Splice,
  TxLogId,
} from '../generated/protocol'
import type { Blobs } from '../blobs'
import { RequestError, type Requester } from '../connection'
import { EditSession, type EditOutcome } from '../compose/session'
import { apply } from '../compose/splice'
import type { Ops } from './ops'
import { Loader, query } from './query'

/** A session editing a box's text in this tab. */
export type RoomDraftSession = EditSession<'text'>

const FIELDS = ['text'] as const

const keyOf = (at: RoomBox) => `${at.room} ${at.thread ?? ''}`

/** What a box never written to holds. */
const empty = (at: RoomBox): RoomDraft => ({ at, revision: 0, text: '', reply_to: null, attachments: [] })

/** A change made here to what a box replies to, or to its files: shown
 * at once, until the drafts, loaded, are as of the entry that made it
 * (`until`; `null` until the server has answered). */
interface Local {
  key: string
  change(draft: RoomDraft | undefined): RoomDraft
  until: TxLogId | null
}

/** A file being uploaded, to attach to a room's box. */
export interface Attaching {
  room: RoomId
  name: string
}

export class RoomDrafts {
  /** The boxes written to, as loaded: `null` until then. */
  private readonly loaded = signal<ReadonlyMap<string, RoomDraft> | null>(null)
  private readonly local = signal<readonly Local[]>([])
  /** The entry the loaded drafts are as of. */
  private asOf: TxLogId | null = null
  /** The boxes written to, by box, with what this tab changed since:
   * `null` until loaded. */
  readonly drafts = computed<ReadonlyMap<string, RoomDraft> | null>(() => {
    const loaded = this.loaded.value
    if (!loaded) return null
    const drafts = new Map(loaded)
    for (const { key, change } of this.local.value) drafts.set(key, change(drafts.get(key)))
    return drafts
  })
  /** The rooms with something to send in a box of theirs. */
  readonly rooms = computed<ReadonlySet<RoomId>>(() => {
    const written = [...(this.drafts.value?.values() ?? [])].filter(
      (d) => d.text.trim() !== '' || d.attachments.length > 0,
    )
    return new Set(written.map((d) => d.at.room))
  })
  /** The files being uploaded. */
  readonly attaching = signal<readonly Attaching[]>([])
  readonly error = signal<string | null>(null)

  private sessions = new Map<string, RoomDraftSession>()
  private loader = new Loader(async () => {
    let response
    try {
      response = await query(this.server, { type: 'list_room_drafts' }, 'room_drafts')
    } catch (error) {
      this.error.value = (error as Error).message
      return
    }
    const asOf = response.as_of
    this.asOf = asOf
    this.loaded.value = new Map(response.drafts.map((d) => [keyOf(d.at), d]))
    this.local.value = this.local.value.filter((l) => l.until === null || l.until > asOf)
  })

  constructor(
    private readonly server: Requester & Blobs,
    private readonly ops: Ops,
  ) {}

  reset() {
    this.loader.invalidate()
  }

  apply(entry: LogEntry) {
    for (const event of entry.events) {
      if (event.type === 'room_draft_edited') this.applyEdit(event)
      else if (event.type === 'room_drafts_changed') this.loader.invalidate()
    }
  }

  private applyEdit(event: RoomDraftEdited) {
    const drafts = this.loaded.value
    // Not loaded yet: the load in flight covers the edit.
    if (!drafts) return
    const key = keyOf(event.at)
    const draft = drafts.get(key) ?? empty(event.at)
    if (event.revision !== draft.revision + 1) {
      throw new Error(`draft ${key} edited at revision ${event.revision}, but known at ${draft.revision}`)
    }
    this.loaded.value = new Map(drafts).set(key, {
      ...draft,
      revision: event.revision,
      text: apply(draft.text, event.splice),
    })
  }

  /** What the box holds; `null` until the drafts are loaded. */
  draft(at: RoomBox): RoomDraft | null {
    const drafts = this.drafts.value
    return drafts && (drafts.get(keyOf(at)) ?? empty(at))
  }

  /** The session editing the box's text in this tab; the drafts must be
   * loaded. */
  session(at: RoomBox): RoomDraftSession {
    const key = keyOf(at)
    let session = this.sessions.get(key)
    if (!session) {
      session = new EditSession<'text'>({
        fields: FIELDS,
        server: () => {
          const drafts = this.loaded.value
          if (!drafts) return undefined
          const { revision, text } = drafts.get(key) ?? empty(at)
          return { revision, content: { text } }
        },
        edit: (revision, _field, splice) => this.edit(at, revision, splice),
      })
      this.sessions.set(key, session)
    }
    return session
  }

  // ── ops ─────────────────────────────────────────────────────────────────

  /** Not through `ops`: edits are saved as typed, without notices, and a
   * conflict is expected now and then. */
  private async edit(at: RoomBox, revision: number, splice: Splice): Promise<EditOutcome> {
    try {
      await this.server.request({ type: 'edit_room_draft', at, revision, splice })
      return 'applied'
    } catch (error) {
      if (error instanceof RequestError && error.error.code === 'conflict') return 'conflict'
      throw error
    }
  }

  /** Has the box's message reply to `event`, or to nothing. */
  setReply(at: RoomBox, event: RoomEvent | null) {
    return this.change(
      at,
      (draft) => ({ ...draft, reply_to: event }),
      { type: 'set_room_draft_reply', at, reply_to: event?.id ?? null },
    )
  }

  /** Uploads `files` and attaches them to the room's box, in order;
   * stops at one that fails. */
  async attach(room: RoomId, files: readonly File[]): Promise<void> {
    const attaching = files.map((file) => ({ room, name: file.name }))
    this.attaching.value = [...this.attaching.value, ...attaching]
    try {
      for (const file of files) {
        let uploaded
        try {
          uploaded = await this.server.upload(file)
        } catch (error) {
          this.ops.fail(`Could not attach ${file.name}: ${(error as Error).message}`)
          return
        }
        const entry = await this.ops.dispatch(
          {
            type: 'attach_to_room_draft',
            room,
            blob: uploaded.blob,
            name: file.name,
            mime: file.type || 'application/octet-stream',
          },
          true,
        )
        if (!entry) return
      }
    } finally {
      this.attaching.value = this.attaching.value.filter((a) => !attaching.includes(a))
    }
  }

  detach(room: RoomId, attachment: DraftAttachmentId) {
    return this.change(
      { room, thread: null },
      (draft) => ({ ...draft, attachments: draft.attachments.filter((a) => a.id !== attachment) }),
      { type: 'detach_from_room_draft', room, attachment },
    )
  }

  /** Changes the box here at once, by `change`, and at the server, by
   * `request`: quietly, as what it changes shows. */
  private async change(at: RoomBox, change: (draft: RoomDraft) => RoomDraft, request: Parameters<Ops['dispatch']>[0]) {
    const local: Local = { key: keyOf(at), change: (draft) => change(draft ?? empty(at)), until: null }
    this.local.value = [...this.local.value, local]
    const entry = await this.ops.dispatch(request, true)
    // Refused, it is as it was; else, until the drafts loaded say so.
    const asOf = this.asOf
    const shown = entry !== null && (asOf === null || asOf < entry.id)
    this.local.value = this.local.value.flatMap((l) => (l !== local ? [l] : shown ? [{ ...l, until: entry.id }] : []))
  }
}
