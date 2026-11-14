// An event of a room's timeline, on a line of its own (SPEC §12.11): its
// time, its sender, and what it says; messages with their reply, media,
// reactions and actions, state events summarized, and anything else as
// its JSON.

import { render } from 'preact'
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
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
import type { MatrixStore } from '../stores/matrix'
import { keepFocus, useEscape } from '../keymap'
import { dateTime, time } from '../format'
import {
  displayName,
  json,
  linkify,
  linkifyMessage,
  links,
  media,
  mxcPath,
  permalink,
  senderColor,
  shown,
  stateSummary,
  TEXT,
  type Content,
  type Permalink,
  type Shown,
} from '../matrix'
import { Confirm } from './Confirm'
import { Icon } from './Icon'
import { MenuButton, type MenuItem } from './Menu'
import { useOpensAbove } from './popover'
import { fileSize } from './size'
import { roomPath } from './Room'
import styles from './Room.module.css'
import overlay from './Overlay.module.css'

/** Of an event, the reactions and edits being sent. */
export type Overlay = readonly MatrixSend[]

/** Reactions offered at once, as Element's quick ones. */
const QUICK = ['👍', '👎', '😄', '🎉', '😕', '❤️', '🚀', '👀']

/** Media are shown at most this large, keeping their aspect ratio. */
const IMAGE_BOX = { width: 320, height: 240 }
const STICKER_BOX = { width: 128, height: 128 }

/** What a line's event is, and the account's to do to it. */
export interface Line {
  room: Room
  event: RoomEvent
  me: string
  overlay: Overlay
  /** Events, and reactions, being deleted. */
  pendingRedactions: ReadonlySet<string>
  reply: () => void
  edit: () => void
  /** Opens the thread the event is the root of, or would be; `null` in
   * a thread. */
  openThread: (() => void) | null
  /** Opens a user's profile, by their user id. */
  openMember: (user: string) => void
}

/** What can be done to a line's event, by its buttons, or by keys while
 * it has the focus: each, but those that cannot. */
export interface LineActions {
  reply?: () => void
  edit?: () => void
  /** Opens its thread, if any. */
  thread?: () => void
  /** Opens the thread it would be the root of. */
  replyInThread?: () => void
  /** Reacts with the key, unless the account has: then takes it back. */
  react?: (key: string) => void
  delete?: () => void
  pin?: { pinned: boolean; toggle: () => void }
}

export function lineActions(line: Line, matrix: MatrixStore): LineActions {
  const { room, event, me } = line
  const summary = stateSummary(event)
  const message = event.kind === 'm.room.message' || event.kind === 'm.sticker'
  const content = shown(event)
  const deleting = line.pendingRedactions.has(event.event_id)
  const own = event.sender === me
  const joined = room.membership === 'joined'
  const replyable = joined && message && !event.redacted
  // A message of a thread is no thread's root, even if its root is not
  // stored.
  const threadable = replyable && line.openThread !== null && content.content['m.relates_to']?.rel_type !== 'm.thread'
  const pinned = room.pinned.includes(event.event_id)
  const actions: LineActions = {}
  if (replyable) {
    actions.reply = line.reply
    actions.react = (key) => {
      const mine = event.reactions
        .find((r) => r.key === key)
        ?.senders.find((s) => s.sender === me && !line.pendingRedactions.has(s.event_id))
      const pending = line.overlay.find((s) => s.content.type === 'reaction' && s.content.key === key)
      if (mine) matrix.send(room.id, { type: 'redaction', event: mine.event_id })
      else if (pending) matrix.cancelSend(pending.id)
      else matrix.send(room.id, { type: 'reaction', event: event.event_id, key })
    }
  }
  if (replyable && own && TEXT.has(content.msgtype ?? '')) actions.edit = line.edit
  if (event.thread !== null && line.openThread !== null) actions.thread = line.openThread
  if (threadable) actions.replyInThread = line.openThread!
  if (own && !event.redacted && !deleting && event.encryption !== 'undecryptable' && summary === null) {
    actions.delete = () => matrix.send(room.id, { type: 'redaction', event: event.event_id })
  }
  if (joined && room.may_pin && message && !event.redacted && !deleting) {
    actions.pin = { pinned, toggle: () => matrix.send(room.id, { type: 'pin', event: event.event_id, pinned: !pinned }) }
  }
  return actions
}

export function EventRow(
  props: Line & {
    /** Members who have read up to this event. */
    readers: readonly MemberReceipt[]
    /** Whether its reaction picker is open, and if by a key, which gives
     * it the focus; and to open or close it. */
    reacting: Reacting
    setReacting: (reacting: Reacting) => void
    /** Whether it is the event replied to or edited, which it shows. */
    target: boolean
    /** Whether deleting it is being asked about; and to ask, or stop. */
    confirming: boolean
    setConfirming: (confirming: boolean) => void
  },
) {
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
    message && !own && !event.redacted && event.highlight && styles.mention,
    deleting && styles.pending,
    props.target && styles.target,
  ]
  const { matrix } = useServices()
  const can = lineActions(props, matrix)
  const row = useRef<HTMLLIElement>(null)
  // Replied to or edited (by Up too), it shows, and is in view.
  useEffect(() => {
    if (props.target) row.current!.scrollIntoView({ block: 'nearest' })
  }, [props.target])
  const menu: MenuItem[] = []
  if (can.delete) menu.push({ label: 'Delete', icon: 'trash', action: () => props.setConfirming(true) })
  if (can.pin) menu.push({ label: can.pin.pinned ? 'Unpin' : 'Pin', icon: 'pin', action: can.pin.toggle })
  menu.push({ label: source ? 'Hide source' : 'View source', icon: 'code', action: () => setSource(!source) })
  const joined = room.membership === 'joined'

  return (
    // Focusable, as the line the keys act on (`Events`).
    <li ref={row} class={classes.filter(Boolean).join(' ')} id={`event-${event.id}`} tabIndex={-1} data-event={event.id}>
      <time class={styles.time} dateTime={new Date(event.ts).toISOString()} title={dateTime(event.ts)}>
        {time(event.ts)}
      </time>
      {summary !== null ? (
        <span class={`${styles.sender} ${styles.muted}`} aria-hidden="true">
          {stateArrow(event)}
        </span>
      ) : (
        <Sender
          room={room}
          user={event.sender}
          name={event.sender_name}
          emote={content.msgtype === 'm.emote'}
          open={() => props.openMember(event.sender)}
        />
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
          <Body room={room} event={event} content={content} overlay={props.overlay} openMember={props.openMember} />
        )}
        {!event.redacted && (
          <Reactions
            event={event}
            me={me}
            overlay={props.overlay}
            pendingRedactions={props.pendingRedactions}
            joined={joined}
            toggle={(key) => can.react?.(key)}
          />
        )}
        {event.thread !== null && props.openThread !== null && (
          <ThreadSummaryButton room={room} summary={event.thread} open={props.openThread} />
        )}
        {source && <pre class={styles.raw}>{pretty(event.raw)}</pre>}
      </div>
      <Readers room={room} readers={props.readers} />
      <div class={styles.actions}>
        {can.react && <ReactButton react={can.react} open={props.reacting} setOpen={props.setReacting} />}
        {can.reply && (
          <button type="button" aria-label="Reply" title="Reply (r)" onMouseDown={keepFocus} onClick={can.reply}>
            <Icon name="reply" />
          </button>
        )}
        {can.replyInThread && (
          <button
            type="button"
            aria-label="Reply in thread"
            title="Reply in thread (t)"
            onMouseDown={keepFocus}
            onClick={can.replyInThread}
          >
            <Icon name="messages-square" />
          </button>
        )}
        {can.edit && (
          <button type="button" aria-label="Edit" title="Edit (e)" onMouseDown={keepFocus} onClick={can.edit}>
            <Icon name="pencil" />
          </button>
        )}
        <MenuButton label="Options" icon="ellipsis-vertical" groups={[menu]} />
      </div>
      {props.confirming && can.delete && (
        <Confirm
          title="Delete message"
          text="It is deleted for everyone in the room; once the homeserver has it, that cannot be undone."
          action="Delete"
          confirm={can.delete}
          close={() => props.setConfirming(false)}
        />
      )}
    </li>
  )
}

/** Members' read receipts, at the line's end, in a column as wide on
 * every line (`--readers-width`): as many avatars as fit, or one fewer
 * and how many more. */
const READERS_SHOWN = 3

function Readers(props: { room: Room; readers: readonly MemberReceipt[] }) {
  const { readers } = props
  if (readers.length === 0) return <span />
  const names = readers.map((r) => displayName(r.member.user, r.member.name))
  const shown = readers.length > READERS_SHOWN ? READERS_SHOWN - 1 : READERS_SHOWN
  return (
    <span class={styles.readers} title={`Read by ${names.join(', ')}`} aria-label={`Read by ${names.join(', ')}`}>
      {readers.slice(0, shown).map((r, i) => (
        <MemberAvatar key={r.member.user} room={props.room} user={r.member.user} name={names[i]} />
      ))}
      {readers.length > shown && <span class={styles.muted}>+{readers.length - shown}</span>}
    </span>
  )
}

/** Before a state event, in the sender's place: what kind of change. */
function stateArrow(event: RoomEvent): string {
  if (event.kind !== 'm.room.member') return '•'
  const membership = json(event).content?.membership
  return membership === 'join' || membership === 'invite' ? '→' : membership === 'leave' || membership === 'ban' ? '←' : '•'
}

/** The sender, whose profile a click opens. */
function Sender(props: { room: Room; user: string; name: string | null; emote: boolean; open: () => void }) {
  const name = displayName(props.user, props.name)
  return (
    <button type="button" class={styles.sender} title={props.user} onClick={props.open}>
      <MemberAvatar room={props.room} user={props.user} name={name} />
      {/* An emote is `* Name does`, as IRC shows it. */}
      <span class={styles.name} style={{ color: senderColor(props.user) }}>
        {props.emote ? '*' : name}
      </span>
    </button>
  )
}

/** Members whose avatars the server had none of (No Content, which an
 * image fails to load), so that their other lines do not ask again. */
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
function Body(props: {
  room: Room
  event: RoomEvent
  content: Shown
  overlay: Overlay
  openMember: (user: string) => void
}) {
  const { room, event, content, openMember } = props
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
        <Text room={room} text={editing.content.body} openMember={openMember} /> <span class={styles.muted}>(saving…)</span>
      </span>
    )
  }
  const edited = event.edit !== null && <Edited room={room} event={event} />
  // Of a thread whose root is not stored, which shows it.
  const orphan =
    event.kind === 'm.room.message' && event.thread_root === null && content.content['m.relates_to']?.rel_type === 'm.thread'
  const tag = orphan && <span class={styles.tag}>in a thread</span>
  if (event.kind === 'm.sticker') {
    return <Media room={room} event={event} content={content} box={STICKER_BOX} openMember={openMember} />
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
          <Media room={room} event={event} content={content} box={IMAGE_BOX} openMember={openMember} /> {edited}
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
        {content.html !== null ? (
          <Html room={room} html={content.html} openMember={openMember} />
        ) : (
          <Text room={room} text={content.body} openMember={openMember} />
        )}{' '}
        {edited}
      </div>
      {!notice && !room.encrypted && <LinkPreviews room={room} event={event} body={content.body} html={content.html} />}
    </>
  )
}

/** Links previewed under a message, at most. */
const PREVIEWS = 2

/** Previews of the pages the message links where it shows, as Element
 * shows them: not of links to Matrix users or rooms, nor in encrypted
 * rooms (the homeserver would learn of the links), nor of notices, which
 * bots send. */
function LinkPreviews(props: { room: Room; event: RoomEvent; body: string; html: string | null }) {
  const urls = [...new Set(links(props.body, props.html).filter((url) => !url.startsWith('https://matrix.to/')))].slice(
    0,
    PREVIEWS,
  )
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
  const image = preview.image === null ? null : mxcPath(preview.image)
  return (
    <a class={styles.preview} href={url} target="_blank" rel="noopener noreferrer">
      {image && (
        <img
          class={styles.previewImage}
          src={`/api/rooms/${room.id}/events/${event.id}/preview/${image}?url=${encodeURIComponent(url)}`}
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

/** Plain text, its URLs linked; those to `matrix.to`, as pills. */
function Text(props: { room: Room; text: string; openMember: (user: string) => void }) {
  const follow = useFollow(props.room, props.openMember)
  return (
    <>
      {linkify(props.text).map((part, i) => {
        if (part.href === null) return part.text
        const link = permalink(part.href)
        return (
          <a
            key={i}
            class={link ? styles.pill : undefined}
            href={part.href}
            target="_blank"
            rel="noopener noreferrer"
            onClick={(event) => link && follow(link) && event.preventDefault()}
          >
            {link ? <PillName room={props.room} link={link} /> : part.text}
          </a>
        )
      })}
    </>
  )
}

/** What a pill of text says: the user's name in the room, or the room's,
 * if the account has it; else the id or alias. */
function PillName(props: { room: Room; link: Permalink }) {
  const { room, link } = props
  const { matrix } = useServices()
  if (link.type === 'user') {
    const member = matrix.members(room.id).state.value?.members.find((m) => m.user === link.id)
    return <>{displayName(link.id, member?.name ?? null)}</>
  }
  return <>{linkedRoom(matrix, room, link.id)?.name ?? link.id}</>
}

/** Of the account's rooms, that of the room id, or of the address, once
 * the homeserver has resolved it. */
function linkedRoom(matrix: MatrixStore, room: Room, id: string): Room | undefined {
  const roomId = id[0] === '#' ? matrix.roomOfAlias(room.account, id) : id
  return matrix.rooms.value!.find((r) => r.account === room.account && r.room_id === roomId)
}

/** Follows a `matrix.to` link where it can: a user's to their profile, a
 * room's to the room, if the account has it (by an address, once
 * resolved). Whether it did; else the link opens matrix.to. */
function useFollow(room: Room, openMember: (user: string) => void): (link: Permalink) => boolean {
  const { matrix } = useServices()
  const { route } = useLocation()
  return (link) => {
    if (link.type === 'user') {
      openMember(link.id)
      return true
    }
    const linked = linkedRoom(matrix, room, link.id)
    if (linked) route(roomPath(linked.id))
    return linked !== undefined
  }
}

/** A message's HTML, as the server sanitized it (SPEC §9.2). Its colours
 * are applied, as the Matrix spec has them (`data-mx-color`), its
 * spoilers revealed on click, and its links to `matrix.to` are pills,
 * followed as `Text`'s. */
function Html(props: { room: Room; html: string; openMember: (user: string) => void }) {
  const root = useRef<HTMLSpanElement>(null)
  const { matrix } = useServices()
  const follow = useFollow(props.room, props.openMember)
  const html = useMemo(() => linkifyMessage(props.html), [props.html])
  useLayoutEffect(() => {
    for (const anchor of root.current!.querySelectorAll('a[href]')) {
      const link = permalink(anchor.getAttribute('href')!)
      if (!link) continue
      anchor.classList.add(styles.pill)
      // Its address resolved, for a click to follow.
      if (link.type === 'room') linkedRoom(matrix, props.room, link.id)
    }
    for (const element of root.current!.querySelectorAll<HTMLElement>('[data-mx-color], [data-mx-bg-color]')) {
      const color = element.dataset.mxColor
      const background = element.dataset.mxBgColor
      if (color && COLOR.test(color)) element.style.color = color
      if (background && COLOR.test(background)) element.style.backgroundColor = background
    }
  }, [html, matrix, props.room])
  // Each code block with a button copying it, as Element's, beside it
  // rather than in it, so that it stays as the block scrolls. Of the HTML
  // only: moving a block resets its scrolling.
  useLayoutEffect(() => {
    const blocks = [...root.current!.querySelectorAll('pre')].map((pre) => {
      const block = document.createElement('div')
      block.className = styles.codeBlock
      pre.replaceWith(block)
      const button = document.createElement('span')
      button.className = styles.copyCode
      block.append(pre, button)
      render(<CopyCode code={pre} />, button)
      return { block, pre, button }
    })
    return () => {
      for (const { block, pre, button } of blocks) {
        render(null, button)
        block.replaceWith(pre)
      }
    }
  }, [html])
  return (
    <span
      ref={root}
      class={styles.html}
      onClick={(event) => {
        const target = event.target as Element
        target.closest('[data-mx-spoiler]')?.classList.add(styles.revealed)
        const link = permalink(target.closest('a[href]')?.getAttribute('href') ?? '')
        if (link && follow(link)) event.preventDefault()
      }}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  )
}

/** Copies the code block's text. */
function CopyCode(props: { code: HTMLElement }) {
  const [copied, setCopied] = useState(false)
  return (
    <button
      type="button"
      aria-label={copied ? 'Copied' : 'Copy code'}
      title={copied ? 'Copied' : 'Copy code'}
      onMouseDown={keepFocus}
      onClick={async () => {
        await navigator.clipboard.writeText(props.code.textContent!)
        setCopied(true)
      }}
      onMouseLeave={() => setCopied(false)}
    >
      <Icon name={copied ? 'check' : 'copy'} />
    </button>
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

export function pretty(raw: string): string {
  return JSON.stringify(JSON.parse(raw), null, 2)
}

/** An image, video, audio or file the message sends. */
function Media(props: {
  room: Room
  event: RoomEvent
  content: Shown
  box: { width: number; height: number }
  openMember: (user: string) => void
}) {
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
          <Html room={room} html={content.html} openMember={props.openMember} />
        ) : (
          <span class={styles.body}>
            <Text room={room} text={content.body!} openMember={props.openMember} />
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

/** Whether a line's reaction picker is open: by the pointer, or by a
 * key (`+`), which gives it the focus (its first reaction; the arrows
 * move along it, and other keys type into its box), and has it back
 * once closed. */
export type Reacting = null | 'pointer' | 'key'

/** The button offering reactions: the quick ones, or any typed. */
function ReactButton(props: {
  react: (key: string) => void
  open: Reacting
  setOpen: (open: Reacting) => void
}) {
  const { open, setOpen } = props
  const [typed, setTyped] = useState('')
  const root = useRef<HTMLSpanElement>(null)
  const input = useRef<HTMLInputElement>(null)
  const picker = useRef<HTMLDivElement>(null)
  const above = useOpensAbove(picker, open !== null)
  const close = () => {
    setOpen(null)
    setTyped('')
    if (open === 'key') root.current!.closest<HTMLElement>('[data-event]')!.focus()
  }
  useLayoutEffect(() => {
    if (open === 'key') picker.current!.querySelector('button')!.focus()
  }, [open])
  useEscape(open !== null ? close : null)
  useEffect(() => {
    if (open === null) return
    const outside = (event: PointerEvent) => {
      if (!root.current!.contains(event.target as Node)) setOpen(null)
    }
    document.addEventListener('pointerdown', outside)
    return () => document.removeEventListener('pointerdown', outside)
  }, [open, setOpen])
  const choose = (key: string) => {
    close()
    props.react(key)
  }
  return (
    <span class={styles.reactRoot} ref={root}>
      <button
        type="button"
        aria-label="React"
        title="React (+)"
        aria-expanded={open !== null}
        onMouseDown={keepFocus}
        onClick={() => setOpen(open === null ? 'pointer' : null)}
      >
        <Icon name="smile-plus" />
      </button>
      {open !== null && (
        <div
          ref={picker}
          class={above ? `${styles.picker} ${styles.above}` : styles.picker}
          role="dialog"
          aria-label="React"
          // Left by the focus (Tab, a click elsewhere), it closes.
          onFocusOut={(event) => {
            if (!picker.current!.contains(event.relatedTarget as Node | null)) setOpen(null)
          }}
          onKeyDown={(event) => {
            const pressed = event.target as HTMLElement
            const arrow = { ArrowLeft: -1, ArrowRight: 1 }[event.key]
            // In the box, the arrows move its caret, but from an empty one.
            if (arrow !== undefined && (pressed !== input.current || typed === '')) {
              const controls = [...picker.current!.querySelectorAll<HTMLElement>('button, input')]
              const at = controls.indexOf(pressed)
              controls[Math.max(0, Math.min(at + arrow, controls.length - 1))].focus()
              event.preventDefault()
            } else if (pressed !== input.current && [...event.key].length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey) {
              // A key typed on a reaction is the box's, not a shortcut.
              event.stopPropagation()
              input.current!.focus()
            }
          }}
        >
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
              ref={input}
              aria-label="Reaction"
              placeholder="Other…"
              value={typed}
              onInput={(event) => setTyped(event.currentTarget.value)}
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
  const { close } = props
  useEscape(close)
  return (
    <div class={overlay.backdrop} onClick={close}>
      <div class={overlay.dialog} role="dialog" aria-label="Edit history" onClick={(e) => e.stopPropagation()}>
        <h2>Edit history</h2>
        <Versions room={props.room} event={props.event} />
      </div>
    </div>
  )
}

/** The message as first sent, then as each edit made it. */
export function Versions(props: { room: Room; event: RoomEvent }) {
  const { room, event } = props
  const { matrix } = useServices()
  const [edits, setEdits] = useState<RoomEvent[] | null>(null)
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
  return edits === null ? (
    <p class={styles.muted}>Loading…</p>
  ) : (
    <ol class={styles.history} aria-label="Versions">
      {versions.map((version, i) => (
        <li key={i}>
          <time class={styles.muted}>{dateTime(version.ts)}</time>
          <div class={styles.body}>{version.body}</div>
        </li>
      ))}
    </ol>
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
