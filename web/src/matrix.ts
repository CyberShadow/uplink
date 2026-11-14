// What Matrix events and sends say, as the views show it (SPEC §9).

import { type Linked, linkify as linkifyText, linkifyHtml } from './linkify'
import type { MatrixContent, MemberAction, Room, RoomEvent, RoomHistory, RoomId, RoomPreview } from './generated/protocol'
import { date } from './format'

/** Message types of text: shown as such, and edited. */
export const TEXT = new Set(['m.text', 'm.notice', 'm.emote'])

/** A message to send, in Markdown unless `markdown` is false. */
export function text(
  body: string,
  options: {
    markdown?: boolean
    emote?: boolean
    reply_to?: string | null
    replaces?: string | null
    thread?: string | null
  } = {},
): MatrixContent {
  return {
    type: 'text',
    body,
    markdown: options.markdown ?? true,
    emote: options.emote ?? false,
    reply_to: options.reply_to ?? null,
    replaces: options.replaces ?? null,
    thread: options.thread ?? null,
  }
}

/** What a send sends, in a few words. */
export function sendSummary(content: MatrixContent): string {
  switch (content.type) {
    case 'text':
      return content.replaces !== null ? `Edit: ${content.body}` : content.body
    case 'reaction':
      return `Reaction ${content.key}`
    case 'redaction':
      return 'Deletion'
    case 'file':
      return content.name
    case 'name':
      return content.name === '' ? 'Name taken away' : `Name: ${content.name}`
    case 'pin':
      return content.pinned ? 'Pin' : 'Unpin'
    case 'topic':
      return content.topic === '' ? 'Topic taken away' : `Topic: ${content.topic}`
    case 'member':
      return `${MEMBER_ACTIONS[content.action]} ${content.user}`
  }
}

/** A change to a member, in a word. */
export const MEMBER_ACTIONS: Record<MemberAction, string> = {
  invite: 'Invite',
  kick: 'Remove',
  ban: 'Ban',
  unban: 'Unban',
}

/** An event's JSON, as far as the views read it. */
export interface EventJson {
  type?: string
  state_key?: string
  content?: Content
  unsigned?: { prev_content?: Content; redacted_because?: { content?: { reason?: string } } }
}

/** Event content: what Matrix says of it is untyped JSON. */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type Content = Record<string, any>

export function json(event: RoomEvent): EventJson {
  return JSON.parse(event.raw) as EventJson
}

/** A message as shown: as last edited. */
export interface Shown {
  msgtype: string | null
  body: string | null
  /** Sanitized by the server. */
  html: string | null
  content: Content
}

export function shown(event: RoomEvent): Shown {
  if (event.edit) {
    const content = JSON.parse(event.edit.content) as Content
    return {
      msgtype: typeof content.msgtype === 'string' ? content.msgtype : null,
      body: typeof content.body === 'string' ? content.body : null,
      html: event.edit.html,
      content,
    }
  }
  return { msgtype: event.msgtype, body: event.body, html: event.html, content: json(event).content ?? {} }
}

/** The `server/id` of media `mxc`, as the server's paths of media take
 * it, if it is a well-formed `mxc:` URI (a server name, of a host and a
 * port, and an id of the Matrix spec's characters): `null` if not. As a
 * path, the browser takes it as it is: of the server names the spec's
 * grammar has, not `.` or `..`, which it would resolve, out of the
 * media's paths to anything of uplink's. */
export function mxcPath(mxc: string): string | null {
  const match = /^mxc:\/\/((?:\[[0-9A-Fa-f:.]+\]|[A-Za-z0-9.-]+)(?::[0-9]{1,5})?)\/([A-Za-z0-9_-]+)$/.exec(mxc)
  if (!match || match[1] === '.' || match[1] === '..') return null
  return `${match[1]}/${match[2]}`
}

/** Where the server serves the media `mxc` an event names (SPEC §9.8),
 * or a thumbnail of it; `null` if it is not an `mxc:` URI. */
export function mediaPath(room: RoomId, event: RoomEvent, mxc: string, thumbnail = false): string | null {
  const path = mxcPath(mxc)
  return path && `/api/rooms/${room}/events/${event.id}/${thumbnail ? 'thumbnail' : 'media'}/${path}`
}

/** The media a message sends, and a thumbnail of it for images and
 * videos: the sender's own, else the homeserver's of a plain image. */
export function media(room: RoomId, event: RoomEvent, content: Content): { full: string; thumbnail: string | null } | null {
  const plain = typeof content.url === 'string'
  const mxc: unknown = plain ? content.url : content.file?.url
  if (typeof mxc !== 'string') return null
  const full = mediaPath(room, event, mxc)
  if (full === null) return null
  const info: Content = content.info ?? {}
  const own: unknown = info.thumbnail_url ?? info.thumbnail_file?.url
  const thumbnail =
    typeof own === 'string'
      ? mediaPath(room, event, own)
      : plain && content.msgtype !== 'm.video'
        ? mediaPath(room, event, mxc, true)
        : null
  return { full, thumbnail }
}

/** Element's palette for senders' names, which it picks by a hash of
 * their user ids: one keeps one's colour in every client. */
const SENDER_COLORS = ['#368bd6', '#ac3ba8', '#03b381', '#e64f7a', '#ff812d', '#2dc2c5', '#5c56f5', '#74d12c']

export function senderColor(user: string): string {
  let hash = 0
  for (let i = 0; i < user.length; i++) hash = (hash * 31 + user.charCodeAt(i)) | 0
  return SENDER_COLORS[Math.abs(hash) % SENDER_COLORS.length]
}

/** What a room's latest message says, in a line (SPEC §12.13): `You`
 * for the account's own. */
export function gist(preview: RoomPreview, me: string): string {
  const who = preview.sender === me ? 'You' : displayName(preview.sender, preview.sender_name)
  if (preview.body === null) return `${who}: ${preview.kind === 'm.room.encrypted' ? 'Unable to decrypt' : preview.kind}`
  const sent = MEDIA[preview.msgtype ?? '']
  if (sent) return `${who} sent ${sent}`
  if (preview.msgtype === 'm.emote') return `* ${who} ${preview.body}`
  // A line: its line breaks are spaces.
  return `${who}: ${preview.body.replace(/\s+/g, ' ')}`
}

const MEDIA: Record<string, string> = {
  'm.image': 'an image',
  'm.video': 'a video',
  'm.audio': 'audio',
  'm.file': 'a file',
}

/** `@alice:example.org`'s name: as the event gives it, else its localpart. */
export function displayName(user: string, name: string | null): string {
  return name ?? (/^@([^:]+)/.exec(user)?.[1] ?? user)
}

/** Of a state event uplink summarizes, what it did, after the sender's
 * name: `joined`, `changed the topic to "…"`; `null` for others. */
export function stateSummary(event: RoomEvent): string | null {
  const raw = json(event)
  if (raw.state_key === undefined) return null
  const content: Content = raw.content ?? {}
  const prev: Content = raw.unsigned?.prev_content ?? {}
  switch (event.kind) {
    case 'm.room.member':
      return membershipSummary(event, raw.state_key, content, prev)
    case 'm.room.name':
      return content.name ? `changed the room name to "${content.name}"` : 'removed the room name'
    case 'm.room.topic':
      return content.topic ? `changed the topic to "${content.topic}"` : 'removed the topic'
    case 'm.room.avatar':
      return content.url ? 'changed the room avatar' : 'removed the room avatar'
    case 'm.room.create':
      return 'created the room'
    case 'm.room.encryption':
      return 'turned on end-to-end encryption'
    case 'm.room.join_rules':
      return `made the room ${content.join_rule === 'public' ? 'public' : `${content.join_rule} only`}`
    case 'm.room.history_visibility':
      return `made history visible to ${HISTORY[content.history_visibility as string] ?? content.history_visibility}`
    case 'm.room.guest_access':
      return content.guest_access === 'can_join' ? 'let guests join' : 'stopped guests from joining'
    case 'm.room.power_levels':
      return 'changed the power levels'
    case 'm.room.canonical_alias':
      return content.alias ? `set the address to ${content.alias}` : 'removed the address'
    case 'm.room.pinned_events':
      return 'changed the pinned messages'
    case 'm.room.server_acl':
      return 'changed the server access list'
    case 'm.room.tombstone':
      return 'replaced this room with a new one'
    case 'm.space.child':
      return Object.keys(content).length > 0 ? 'added a room to the space' : 'removed a room from the space'
    case 'm.space.parent':
      return 'changed the spaces it is in'
    default:
      return null
  }
}

const HISTORY: Record<string, string> = {
  world_readable: 'anyone',
  shared: 'all members, from the start',
  invited: 'members, from when they were invited',
  joined: 'members, from when they joined',
}

function membershipSummary(event: RoomEvent, target: string, content: Content, prev: Content): string {
  const name = displayName(target, content.displayname ?? prev.displayname ?? null)
  const reason = content.reason ? `: ${content.reason}` : ''
  const self = target === event.sender
  switch (content.membership) {
    case 'join':
      if (prev.membership !== 'join') return 'joined'
      if (content.displayname !== prev.displayname) {
        if (!content.displayname) return 'removed their display name'
        if (!prev.displayname) return `set their display name to ${content.displayname}`
        return `changed their display name to ${content.displayname}`
      }
      if (content.avatar_url !== prev.avatar_url) return content.avatar_url ? 'changed their avatar' : 'removed their avatar'
      return 'made no change'
    case 'invite':
      return `invited ${name}`
    case 'leave':
      if (prev.membership === 'invite') return self ? 'declined the invitation' : `withdrew ${name}'s invitation${reason}`
      if (prev.membership === 'ban') return `unbanned ${name}`
      return self ? `left${reason}` : `removed ${name}${reason}`
    case 'ban':
      return `banned ${name}${reason}`
    case 'knock':
      return 'asked to join'
    default:
      return `changed ${name}'s membership`
  }
}

/** A message's plain text, in parts: links to what look like web and
 * email addresses, and to the users and rooms of addresses of theirs
 * (`@alice:example.org`, `#room:example.org`), as `matrix.to` links,
 * which are shown as pills; and the rest. */
export function linkify(text: string): Linked {
  const parts: Linked = []
  let last = 0
  for (const match of text.matchAll(ADDRESS)) {
    parts.push(...linkifyText(text.slice(last, match.index)))
    parts.push({ text: match[0], href: `https://matrix.to/#/${encodeURIComponent(match[0])}` })
    last = match.index + match[0].length
  }
  parts.push(...linkifyText(text.slice(last)))
  return parts
}

/** A user's or room's address, as a word of its own (not in a URL): of
 * a server by its name, with a dot, or by its IPv6 address, and its port,
 * if any. Linkify would take the server's name for a web address. */
const ADDRESS = /(?<=^|[\s(<[{"'])[@#][^\s:]+:(?:[\dA-Za-z-]+(?:\.[\dA-Za-z-]+)+|\[[\dA-Fa-f:.]+\])(?::\d+)?/g

/** A message's HTML, its text linked as `linkify` links it. */
export function linkifyMessage(html: string): string {
  return linkifyHtml(html, linkify)
}

/** A `matrix.to` link to the room: by its address, if it has one;
 * else by its id, by way of the account's homeserver (`me`'s), which is
 * in it, as a room id of version 12 names none. */
export function roomLink(room: { room_id: string; alias: string | null }, me: string): string {
  if (room.alias !== null) return `https://matrix.to/#/${encodeURIComponent(room.alias)}`
  const via = me.slice(me.indexOf(':') + 1)
  return `https://matrix.to/#/${encodeURIComponent(room.room_id)}?via=${encodeURIComponent(via)}`
}

/** What a `matrix.to` link names: a user, or a room, by its id or an
 * alias; `null` of other links, and of links to an event. */
export type Permalink = { type: 'user'; id: string } | { type: 'room'; id: string }

export function permalink(href: string): Permalink | null {
  const target = /^https:\/\/matrix\.to\/#\/([^/?]+)(?:\?.*)?$/.exec(href)?.[1]
  if (target === undefined) return null
  let id
  try {
    id = decodeURIComponent(target)
  } catch {
    // Not a link Matrix clients make: a plain one.
    return null
  }
  switch (id[0]) {
    case '@':
      return { type: 'user', id }
    case '!':
    case '#':
      return { type: 'room', id }
    default:
      return null
  }
}

/** The web pages a message links, as it is shown: of its HTML, if it
 * has any, its links; else what its text links. Its text may not be what
 * the HTML says: bridges put their network's markup there (Slack's
 * `<http://dlang.org|dlang.org>`). */
export function links(body: string, html: string | null): string[] {
  const hrefs =
    html === null
      ? linkify(body).flatMap((part) => (part.href === null ? [] : [part.href]))
      : [...new DOMParser().parseFromString(linkifyMessage(html), 'text/html').querySelectorAll('a[href]')].map(
          (a) => a.getAttribute('href')!,
        )
  return hrefs.filter((href) => /^https?:\/\//.test(href))
}

/** How far fetching the whole history of an account's rooms has got
 * (SPEC §9.2): how many rooms are still being fetched, back to when all
 * of them are stored, as the latest history is fetched first, and so what
 * part of the time back to the oldest one's creation is fetched, if any
 * one's is known. */
export interface HistoryProgress {
  rooms: number
  since: number
  percent: number | null
}

/** Of an account's rooms' histories, how far fetching them has got, as of
 * `now`; `null` if none is being fetched. Up to 99% until done. */
export function historyProgress(histories: readonly RoomHistory[], now: number): HistoryProgress | null {
  const fetching = histories.flatMap((history) => (history.type === 'fetching' ? [history] : []))
  if (fetching.length === 0) return null
  const since = Math.max(...fetching.map((h) => h.since))
  const created = fetching.flatMap((h) => (h.created === null ? [] : [h.created]))
  const percent =
    created.length === 0
      ? null
      : Math.min(99, Math.floor(((now - since) / Math.max(now - Math.min(...created), 1)) * 100))
  return { rooms: fetching.length, since, percent }
}

/** What of a room's history is stored, and so searched; `null` if all. */
export function storedHistory(room: Room): string | null {
  const { history } = room
  switch (history.type) {
    case 'whole':
      return null
    case 'fetching':
      return `stored since ${date(new Date(history.since))}; the rest is being fetched`
    case 'partial':
      return `stored since ${date(new Date(history.since))}; before, only what was scrolled to`
  }
}

