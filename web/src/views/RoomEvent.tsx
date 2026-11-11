// An event of a room's timeline, on a line of its own (SPEC §12.11): its
// time, its sender, and what it says; messages with their reply, media,
// reactions and actions, state events summarized, and anything else as
// its JSON.

import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks'
import { tinykeys } from 'tinykeys'
import type {
  MatrixSend,
  MemberReceipt,
  Room,
  RoomEvent,
  RepliedEvent,
  RoomEventId,
  ThreadSummary,
  UrlPreview,
} from '../generated/protocol'
import { useServices } from '../context'
import { keepFocus } from '../keymap'
import { dateTime, time } from '../format'
import {
  displayName,
  json,
  linkify,
  media,
  mentions,
  senderColor,
  shown,
  stateSummary,
  TEXT,
  type Content,
  type Shown,
} from '../matrix'
import { Icon } from './Icon'
import { MenuButton, type MenuItem } from './Menu'
import { fileSize } from './size'
import styles from './Room.module.css'
import overlay from './Overlay.module.css'

/** Of an event, the reactions and edits being sent. */
export type Overlay = readonly MatrixSend[]

/** Reactions offered at once, as Element's quick ones. */
const QUICK = ['👍', '👎', '😄', '🎉', '😕', '❤️', '🚀', '👀']

/** Media are shown at most this large, keeping their aspect ratio. */
const IMAGE_BOX = { width: 320, height: 240 }
const STICKER_BOX = { width: 128, height: 128 }

export function EventRow(props: {
  room: Room
  event: RoomEvent
  me: string
  overlay: Overlay
  /** Members who have read up to this event. */
  readers: readonly MemberReceipt[]
  /** Events, and reactions, being deleted. */
  pendingRedactions: ReadonlySet<string>
  reply: () => void
  edit: () => void
  /** Opens the thread the event is the root of, or would be; `null` in
   * a thread. */
  openThread: (() => void) | null
}) {
  const { room, event, me } = props
  const [source, setSource] = useState(false)
  const summary = stateSummary(event)
  const message = event.kind === 'm.room.message' || event.kind === 'm.sticker'
  const content = shown(event)
  const deleting = props.pendingRedactions.has(event.event_id)
  const own = event.sender === me
  const classes = [
    styles.row,
    summary !== null && styles.state,
    message && !own && !event.redacted && mentions(content.content, me) && styles.mention,
    deleting && styles.pending,
  ]
  const { matrix } = useServices()
  const actions: MenuItem[] = []
  if (own && !event.redacted && !deleting && event.encryption !== 'undecryptable' && summary === null) {
    actions.push({
      label: 'Delete',
      icon: 'trash',
      action: () => matrix.send(room.id, { type: 'redaction', event: event.event_id }),
    })
  }
  if (room.membership === 'joined' && room.may_pin && message && !event.redacted && !deleting) {
    const pinned = room.pinned.includes(event.event_id)
    actions.push({
      label: pinned ? 'Unpin' : 'Pin',
      icon: 'pin',
      action: () => matrix.send(room.id, { type: 'pin', event: event.event_id, pinned: !pinned }),
    })
  }
  actions.push({ label: source ? 'Hide source' : 'View source', icon: 'code', action: () => setSource(!source) })
  // Reacts with `key`, unless the account has already: then takes it back.
  const toggle = (key: string) => {
    const mine = event.reactions
      .find((r) => r.key === key)
      ?.senders.find((s) => s.sender === me && !props.pendingRedactions.has(s.event_id))
    const pending = props.overlay.find((s) => s.content.type === 'reaction' && s.content.key === key)
    if (mine) matrix.send(room.id, { type: 'redaction', event: mine.event_id })
    else if (pending) matrix.cancelSend(pending.id)
    else matrix.send(room.id, { type: 'reaction', event: event.event_id, key })
  }
  const joined = room.membership === 'joined'
  const editable = own && message && !event.redacted && TEXT.has(content.msgtype ?? '')
  const replyable = message && !event.redacted
  // A message of a thread is no thread's root, even if its root is not
  // stored.
  const threadable = replyable && props.openThread !== null && content.content['m.relates_to']?.rel_type !== 'm.thread'

  return (
    <li class={classes.filter(Boolean).join(' ')} id={`event-${event.id}`}>
      <time class={styles.time} dateTime={new Date(event.ts).toISOString()} title={dateTime(event.ts)}>
        {time(event.ts)}
      </time>
      {summary !== null ? (
        <span class={`${styles.sender} ${styles.muted}`} aria-hidden="true">
          {stateArrow(event)}
        </span>
      ) : (
        <Sender room={room} user={event.sender} name={event.sender_name} emote={content.msgtype === 'm.emote'} />
      )}
      <div class={styles.content}>
        {event.replied && !event.redacted && <ReplyPreview replied={event.replied} />}
        {deleting ? (
          <span class={styles.muted}>Deleting…</span>
        ) : summary !== null ? (
          <span class={styles.muted}>
            {displayName(event.sender, event.sender_name)} {summary}
          </span>
        ) : (
          <Body room={room} event={event} content={content} overlay={props.overlay} />
        )}
        {!event.redacted && (
          <Reactions
            event={event}
            me={me}
            overlay={props.overlay}
            pendingRedactions={props.pendingRedactions}
            joined={joined}
            toggle={toggle}
          />
        )}
        {event.thread !== null && props.openThread !== null && (
          <ThreadSummaryButton room={room} summary={event.thread} open={props.openThread} />
        )}
        {source && <pre class={styles.raw}>{pretty(event.raw)}</pre>}
      </div>
      <Readers room={room} readers={props.readers} />
      <div class={styles.actions}>
        {joined && replyable && <ReactButton react={toggle} />}
        {joined && replyable && (
          <button type="button" aria-label="Reply" title="Reply" onMouseDown={keepFocus} onClick={props.reply}>
            <Icon name="reply" />
          </button>
        )}
        {joined && threadable && (
          <button
            type="button"
            aria-label="Reply in thread"
            title="Reply in thread"
            onMouseDown={keepFocus}
            onClick={props.openThread!}
          >
            <Icon name="messages-square" />
          </button>
        )}
        {joined && editable && (
          <button type="button" aria-label="Edit" title="Edit" onMouseDown={keepFocus} onClick={props.edit}>
            <Icon name="pencil" />
          </button>
        )}
        <MenuButton label="Options" icon="ellipsis-vertical" groups={[actions]} />
      </div>
    </li>
  )
}

/** Members' read receipts, at the line's end: a few avatars, and how
 * many more. */
const READERS_SHOWN = 3

function Readers(props: { room: Room; readers: readonly MemberReceipt[] }) {
  const { readers } = props
  if (readers.length === 0) return <span />
  const names = readers.map((r) => displayName(r.member.user, r.member.name))
  return (
    <span class={styles.readers} title={`Read by ${names.join(', ')}`} aria-label={`Read by ${names.join(', ')}`}>
      {readers.slice(0, READERS_SHOWN).map((r) => (
        <MemberAvatar key={r.member.user} room={props.room} user={r.member.user} name={displayName(r.member.user, r.member.name)} />
      ))}
      {readers.length > READERS_SHOWN && <span class={styles.muted}>+{readers.length - READERS_SHOWN}</span>}
    </span>
  )
}

/** Before a state event, in the sender's place: what kind of change. */
function stateArrow(event: RoomEvent): string {
  if (event.kind !== 'm.room.member') return '•'
  const membership = json(event).content?.membership
  return membership === 'join' || membership === 'invite' ? '→' : membership === 'leave' || membership === 'ban' ? '←' : '•'
}

function Sender(props: { room: Room; user: string; name: string | null; emote: boolean }) {
  const name = displayName(props.user, props.name)
  return (
    <span class={styles.sender} title={props.user}>
      <MemberAvatar room={props.room} user={props.user} name={name} />
      {/* An emote is `* Name does`, as IRC shows it. */}
      <span class={styles.name} style={{ color: senderColor(props.user) }}>
        {props.emote ? '*' : name}
      </span>
    </span>
  )
}

/** Members whose avatars the server had none of, so that their other
 * lines do not ask again. */
const noAvatar = new Set<string>()

/** The member's avatar in the room, or their initial; `class` sizes it. */
export function MemberAvatar(props: { room: Room; user: string; name: string; class?: string }) {
  const cls = props.class ? `${styles.memberAvatar} ${props.class}` : styles.memberAvatar
  const key = `${props.room.id} ${props.user}`
  const [failed, setFailed] = useState(noAvatar.has(key))
  if (failed) {
    return (
      <span class={cls} style={{ background: senderColor(props.user) }} aria-hidden="true">
        {(props.name.match(/\p{L}|\p{N}/u)?.[0] ?? '?').toUpperCase()}
      </span>
    )
  }
  return (
    <img
      class={cls}
      src={`/api/rooms/${props.room.id}/members/${encodeURIComponent(props.user)}/avatar`}
      alt=""
      loading="lazy"
      onError={() => {
        noAvatar.add(key)
        setFailed(true)
      }}
    />
  )
}

/** What a message says, as last edited, or being edited. */
function Body(props: { room: Room; event: RoomEvent; content: Shown; overlay: Overlay }) {
  const { room, event, content } = props
  if (event.redacted) {
    const reason = json(event).unsigned?.redacted_because?.content?.reason
    return <span class={styles.muted}>Message deleted{reason ? `: ${reason}` : ''}</span>
  }
  if (event.encryption === 'undecryptable') {
    return <span class={styles.muted}>Unable to decrypt this message.</span>
  }
  const editing = props.overlay.findLast((s) => s.content.type === 'text')
  if (editing?.content.type === 'text') {
    return (
      <span class={`${styles.body} ${styles.saving}`}>
        <Text text={editing.content.body} /> <span class={styles.muted}>(saving…)</span>
      </span>
    )
  }
  const edited = event.edit !== null && <Edited room={room} event={event} />
  // Of a thread whose root is not stored, which shows it.
  const orphan =
    event.kind === 'm.room.message' && event.thread_root === null && content.content['m.relates_to']?.rel_type === 'm.thread'
  const tag = orphan && <span class={styles.tag}>in a thread</span>
  if (event.kind === 'm.sticker') {
    return <Media room={room} event={event} content={content} box={STICKER_BOX} />
  }
  if (event.kind !== 'm.room.message') return <Raw event={event} />
  switch (content.msgtype) {
    case 'm.image':
    case 'm.video':
    case 'm.audio':
    case 'm.file':
      return (
        <>
          {tag}
          <Media room={room} event={event} content={content} box={IMAGE_BOX} /> {edited}
        </>
      )
    case 'm.location':
      return (
        <span class={styles.body}>
          {tag}
          <Location content={content.content} /> {edited}
        </span>
      )
  }
  // Others, whatever their type, as their text: clients should show it.
  if (content.body === null) return <Raw event={event} />
  const notice = content.msgtype === 'm.notice'
  const emote = content.msgtype === 'm.emote'
  return (
    <>
      <div class={[styles.body, notice && styles.notice].filter(Boolean).join(' ')}>
        {tag}
        {emote && <span style={{ color: senderColor(event.sender) }}>{displayName(event.sender, event.sender_name)} </span>}
        {content.html !== null ? <Html html={content.html} /> : <Text text={content.body} />} {edited}
      </div>
      {!notice && !room.encrypted && <LinkPreviews room={room} event={event} body={content.body} />}
    </>
  )
}

/** Links previewed under a message, at most. */
const PREVIEWS = 2

/** Previews of the pages the message's text links, as Element shows them:
 * not of links to Matrix users or rooms, nor in encrypted rooms (the
 * homeserver would learn of the links), nor of notices, which bots send. */
function LinkPreviews(props: { room: Room; event: RoomEvent; body: string }) {
  const urls = [
    ...new Set(
      linkify(props.body)
        .flatMap((part) => (part.href === null ? [] : [part.href]))
        .filter((url) => !url.startsWith('https://matrix.to/')),
    ),
  ].slice(0, PREVIEWS)
  return (
    <>
      {urls.map((url) => (
        <LinkPreview key={url} room={props.room} event={props.event} url={url} />
      ))}
    </>
  )
}

function LinkPreview(props: { room: Room; event: RoomEvent; url: string }) {
  const { room, event, url } = props
  const { matrix } = useServices()
  const [preview, setPreview] = useState<UrlPreview | null>(null)
  useEffect(() => {
    let current = true
    matrix.preview(room.id, event.id, url).then((p) => current && setPreview(p))
    return () => {
      current = false
    }
  }, [matrix, room.id, event.id, url])
  if (preview === null) return null
  const image = preview.image?.match(/^mxc:\/\/([^/]+)\/([^/?#]+)$/)
  return (
    <a class={styles.preview} href={url} target="_blank" rel="noopener noreferrer">
      {image && (
        <img
          class={styles.previewImage}
          src={`/api/rooms/${room.id}/events/${event.id}/preview/${image[1]}/${image[2]}?url=${encodeURIComponent(url)}`}
          alt=""
          loading="lazy"
        />
      )}
      <span class={styles.previewText}>
        {preview.site_name !== null && preview.site_name !== preview.title && (
          <span class={styles.muted}>{preview.site_name}</span>
        )}
        {preview.title !== null && <strong>{preview.title}</strong>}
        {preview.description !== null && <span class={styles.previewDescription}>{preview.description}</span>}
      </span>
    </a>
  )
}

/** Plain text, its URLs linked. */
function Text(props: { text: string }) {
  return (
    <>
      {linkify(props.text).map((part, i) =>
        part.href === null ? (
          part.text
        ) : (
          <a key={i} href={part.href} target="_blank" rel="noopener noreferrer">
            {part.text}
          </a>
        ),
      )}
    </>
  )
}

/** A message's HTML, as the server sanitized it (SPEC §9.2). Its colours
 * are applied, as the Matrix spec has them (`data-mx-color`), and its
 * spoilers revealed on click. */
function Html(props: { html: string }) {
  const root = useRef<HTMLSpanElement>(null)
  useLayoutEffect(() => {
    for (const element of root.current!.querySelectorAll<HTMLElement>('[data-mx-color], [data-mx-bg-color]')) {
      const color = element.dataset.mxColor
      const background = element.dataset.mxBgColor
      if (color && COLOR.test(color)) element.style.color = color
      if (background && COLOR.test(background)) element.style.backgroundColor = background
    }
  }, [props.html])
  return (
    <span
      ref={root}
      class={styles.html}
      onClick={(event) => {
        const spoiler = (event.target as Element).closest('[data-mx-spoiler]')
        spoiler?.classList.add(styles.revealed)
      }}
      dangerouslySetInnerHTML={{ __html: props.html }}
    />
  )
}

/** The colours the Matrix spec lets messages set. */
const COLOR = /^#[0-9a-fA-F]{6}$/

function Raw(props: { event: RoomEvent }) {
  const { event } = props
  return (
    <details>
      <summary>
        <code>{event.msgtype ? `${event.kind} (${event.msgtype})` : event.kind}</code>
      </summary>
      <pre class={styles.raw}>{pretty(event.raw)}</pre>
    </details>
  )
}

function pretty(raw: string): string {
  return JSON.stringify(JSON.parse(raw), null, 2)
}

/** An image, video, audio or file the message sends. */
function Media(props: { room: Room; event: RoomEvent; content: Shown; box: { width: number; height: number } }) {
  const { room, event, content } = props
  const found = media(room.id, event, content.content)
  const info: Content = content.content.info ?? {}
  const name: string = content.content.filename ?? content.body ?? 'file'
  // A body other than the file's name is a caption.
  const caption = content.content.filename !== undefined && content.body !== null && content.body !== name
  if (found === null) return <Raw event={event} />
  const size = fitted(info, props.box)
  let shownMedia
  if (event.kind === 'm.sticker' || content.msgtype === 'm.image') {
    shownMedia = (
      <a href={found.full} target="_blank" rel="noopener noreferrer" class={styles.media} title={name}>
        <img src={found.thumbnail ?? found.full} alt={content.body ?? name} loading="lazy" {...size} />
      </a>
    )
  } else if (content.msgtype === 'm.video') {
    shownMedia = (
      <video
        class={styles.media}
        controls
        preload="none"
        src={found.full}
        poster={found.thumbnail ?? undefined}
        aria-label={name}
        {...size}
      />
    )
  } else if (content.msgtype === 'm.audio') {
    shownMedia = <audio class={styles.media} controls preload="none" src={found.full} aria-label={name} />
  } else {
    shownMedia = (
      <a href={found.full} download={name} class={styles.file}>
        <Icon name="download" />
        {name}
        {typeof info.size === 'number' && <span class={styles.muted}> ({fileSize(info.size)})</span>}
      </a>
    )
  }
  return (
    <span class={styles.mediaBlock}>
      {shownMedia}
      {caption &&
        (content.html !== null ? (
          <Html html={content.html} />
        ) : (
          <span class={styles.body}>
            <Text text={content.body!} />
          </span>
        ))}
    </span>
  )
}

/** The size media are shown at, within `box`, as the event says they are. */
function fitted(info: Content, box: { width: number; height: number }): { width?: number; height?: number } {
  const { w, h } = info
  if (typeof w !== 'number' || typeof h !== 'number' || w <= 0 || h <= 0) return {}
  const scale = Math.min(1, box.width / w, box.height / h)
  return { width: Math.round(w * scale), height: Math.round(h * scale) }
}

/** A location, as a link to it on OpenStreetMap. */
function Location(props: { content: Content }) {
  const { content } = props
  const at = /^geo:(-?[\d.]+),(-?[\d.]+)/.exec(content.geo_uri ?? '')
  if (!at) return <>{content.body}</>
  const [, lat, lon] = at
  return (
    <a href={`https://www.openstreetmap.org/?mlat=${lat}&mlon=${lon}#map=16/${lat}/${lon}`} target="_blank" rel="noopener noreferrer">
      📍 {content.body ?? `${lat}, ${lon}`}
    </a>
  )
}

/** What a reply answers, above it; a click shows it, if loaded. */
function ReplyPreview(props: { replied: RepliedEvent }) {
  const { replied } = props
  const body = replied.redacted ? 'Message deleted' : (replied.body ?? replied.kind)
  return (
    <button
      type="button"
      class={styles.reply}
      onMouseDown={keepFocus}
      onClick={() => reveal(replied.id)}
    >
      <Icon name="reply" />
      <span style={{ color: senderColor(replied.sender) }}>{displayName(replied.sender, replied.sender_name)}</span>{' '}
      <span class={styles.quoted}>{body}</span>
    </button>
  )
}

/** Under a thread's root, as Element's: how many replies, and the latest. */
function ThreadSummaryButton(props: { room: Room; summary: ThreadSummary; open: () => void }) {
  const { latest, replies } = props.summary
  const name = displayName(latest.sender, latest.sender_name)
  return (
    <button type="button" class={styles.threadSummary} onMouseDown={keepFocus} onClick={props.open}>
      <Icon name="messages-square" />
      <span class={styles.threadCount}>{replies === 1 ? '1 reply' : `${replies} replies`}</span>
      <MemberAvatar room={props.room} user={latest.sender} name={name} />
      <span style={{ color: senderColor(latest.sender) }}>{name}</span>
      <span class={styles.quoted}>{latest.redacted ? 'Message deleted' : (latest.body ?? latest.kind)}</span>
    </button>
  )
}

/** Scrolls to the event's line, if on show, and highlights it a while. */
export function reveal(event: RoomEventId) {
  const target = document.getElementById(`event-${event}`)
  if (!target) return
  target.scrollIntoView({ block: 'center' })
  target.classList.add(styles.highlight)
  setTimeout(() => target.classList.remove(styles.highlight), 2000)
}

/** The reactions to the event: a click on one adds the account's own, or
 * takes it back. Those being sent count, and those being taken back do
 * not. */
function Reactions(props: {
  event: RoomEvent
  me: string
  overlay: Overlay
  pendingRedactions: ReadonlySet<string>
  joined: boolean
  toggle: (key: string) => void
}) {
  const { event, me } = props
  const sending = props.overlay.filter((s) => s.content.type === 'reaction')
  const keys = [...event.reactions.map((r) => r.key)]
  for (const item of sending) {
    if (item.content.type === 'reaction' && !keys.includes(item.content.key)) keys.push(item.content.key)
  }
  if (keys.length === 0) return null
  return (
    <ul class={styles.reactions} aria-label="Reactions">
      {keys.map((key) => {
        const senders = (event.reactions.find((r) => r.key === key)?.senders ?? []).filter(
          (s) => !props.pendingRedactions.has(s.event_id),
        )
        const mine = senders.find((s) => s.sender === me)
        const pending = sending.find((s) => s.content.type === 'reaction' && s.content.key === key)
        const count = senders.length + (pending && !mine ? 1 : 0)
        if (count === 0) return null
        const names = senders.map((s) => displayName(s.sender, s.sender_name))
        if (pending && !mine) names.push('you')
        return (
          <li key={key}>
            <button
              type="button"
              class={[styles.reaction, (mine || pending) && styles.own, pending && styles.saving].filter(Boolean).join(' ')}
              aria-pressed={mine !== undefined || pending !== undefined}
              title={`${names.join(', ')} reacted with ${key}`}
              disabled={!props.joined}
              onMouseDown={keepFocus}
              onClick={() => props.toggle(key)}
            >
              {key} {count}
            </button>
          </li>
        )
      })}
    </ul>
  )
}

/** The button offering reactions: the quick ones, or any typed. */
function ReactButton(props: { react: (key: string) => void }) {
  const [open, setOpen] = useState(false)
  const [typed, setTyped] = useState('')
  const root = useRef<HTMLSpanElement>(null)
  useEffect(() => {
    if (!open) return
    const outside = (event: PointerEvent) => {
      if (!root.current!.contains(event.target as Node)) setOpen(false)
    }
    document.addEventListener('pointerdown', outside)
    return () => document.removeEventListener('pointerdown', outside)
  }, [open])
  const choose = (key: string) => {
    setOpen(false)
    setTyped('')
    props.react(key)
  }
  return (
    <span class={styles.reactRoot} ref={root}>
      <button
        type="button"
        aria-label="React"
        title="React"
        aria-expanded={open}
        onMouseDown={keepFocus}
        onClick={() => setOpen(!open)}
      >
        <Icon name="smile-plus" />
      </button>
      {open && (
        <div class={styles.picker} role="dialog" aria-label="React">
          {QUICK.map((key) => (
            <button key={key} type="button" aria-label={`React with ${key}`} onMouseDown={keepFocus} onClick={() => choose(key)}>
              {key}
            </button>
          ))}
          <form
            onSubmit={(event) => {
              event.preventDefault()
              if (typed.trim() !== '') choose(typed.trim())
            }}
          >
            <input
              aria-label="Reaction"
              placeholder="Other…"
              value={typed}
              onInput={(event) => setTyped(event.currentTarget.value)}
              onKeyDown={(event) => {
                if (event.key !== 'Escape') return
                event.stopPropagation()
                setOpen(false)
              }}
            />
          </form>
        </div>
      )}
    </span>
  )
}

/** That the message was edited; a click shows how. */
function Edited(props: { room: Room; event: RoomEvent }) {
  const [open, setOpen] = useState(false)
  return (
    <>
      <button
        type="button"
        class={styles.edited}
        title={`Edited ${dateTime(props.event.edit!.ts)}`}
        onMouseDown={keepFocus}
        onClick={() => setOpen(true)}
      >
        (edited)
      </button>
      {open && <EditHistory room={props.room} event={props.event} close={() => setOpen(false)} />}
    </>
  )
}

/** The message as first sent, then as each edit made it. */
function EditHistory(props: { room: Room; event: RoomEvent; close: () => void }) {
  const { room, event, close } = props
  const { matrix } = useServices()
  const [edits, setEdits] = useState<RoomEvent[] | null>(null)
  useEffect(() => tinykeys(window, { Escape: close }), [close])
  useEffect(() => {
    let current = true
    matrix.edits(room.id, event.id).then((edits) => {
      if (current && edits) setEdits(edits)
    })
    return () => {
      current = false
    }
  }, [matrix, room.id, event.id])
  const versions: Array<{ ts: number; body: string }> = [{ ts: event.ts, body: event.body ?? '' }]
  for (const edit of edits ?? []) {
    const body = json(edit).content?.['m.new_content']?.body
    versions.push({ ts: edit.ts, body: typeof body === 'string' ? body : '' })
  }
  return (
    <div class={overlay.backdrop} onClick={close}>
      <div class={overlay.dialog} role="dialog" aria-label="Edit history" onClick={(e) => e.stopPropagation()}>
        <h2>Edit history</h2>
        {edits === null ? (
          <p class={styles.muted}>Loading…</p>
        ) : (
          <ol class={styles.history}>
            {versions.map((version, i) => (
              <li key={i}>
                <time class={styles.muted}>{dateTime(version.ts)}</time>
                <div class={styles.body}>{version.body}</div>
              </li>
            ))}
          </ol>
        )}
      </div>
    </div>
  )
}

/** Membership changes in a row, folded into a line, as Element folds
 * them; unfolded on click. */
export function MembershipRun(props: { room: Room; events: readonly RoomEvent[] }) {
  const { events } = props
  const said = events.map((e) => `${displayName(e.sender, e.sender_name)} ${stateSummary(e) ?? 'changed membership'}`)
  const first = events[0]
  return (
    <li class={`${styles.row} ${styles.state}`}>
      <time class={styles.time} dateTime={new Date(first.ts).toISOString()} title={dateTime(first.ts)}>
        {time(first.ts)}
      </time>
      <span class={`${styles.sender} ${styles.muted}`} aria-hidden="true">
        •
      </span>
      <details class={styles.content}>
        <summary class={styles.muted}>
          {events.length} membership changes: {said.slice(0, 2).join(', ')}…
        </summary>
        <ul class={styles.run}>
          {events.map((e, i) => (
            <li key={e.id}>
              <time class={styles.muted} title={dateTime(e.ts)}>
                {time(e.ts)}
              </time>{' '}
              <span class={styles.muted}>{said[i]}</span>
            </li>
          ))}
        </ul>
      </details>
    </li>
  )
}
