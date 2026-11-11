// What Matrix events and sends say, as the views show it (SPEC §9).

import type { MatrixContent, MemberAction, RoomEvent, RoomId } from './generated/protocol'

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

/** Where the server serves the media `mxc` an event names (SPEC §9.8),
 * or a thumbnail of it; `null` if it is not an `mxc:` URI. */
export function mediaPath(room: RoomId, event: RoomEvent, mxc: string, thumbnail = false): string | null {
  const match = /^mxc:\/\/([^/]+)\/([^/?#]+)$/.exec(mxc)
  if (!match) return null
  const [, server, media] = match
  return `/api/rooms/${room}/events/${event.id}/${thumbnail ? 'thumbnail' : 'media'}/${encodeURIComponent(server)}/${encodeURIComponent(media)}`
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

/** Plain text, in parts: links to what look like URLs, and the rest. */
export function linkify(text: string): Array<{ text: string; href: string | null }> {
  const parts: Array<{ text: string; href: string | null }> = []
  let last = 0
  for (const match of text.matchAll(URL)) {
    // Not the punctuation that ends the sentence it is in.
    const url = match[0].replace(/[.,:;!?)\]'"]+$/, '')
    if (match.index > last) parts.push({ text: text.slice(last, match.index), href: null })
    parts.push({ text: url, href: url })
    last = match.index + url.length
  }
  if (last < text.length) parts.push({ text: text.slice(last), href: null })
  return parts
}

const URL = /\bhttps?:\/\/[^\s<>]+/g

/** Whether the message mentions `user`: by `m.mentions`, or, of senders
 * that do not say, by naming them. */
export function mentions(content: Content, user: string): boolean {
  const said = content['m.mentions']
  if (said !== undefined) return Array.isArray(said.user_ids) && said.user_ids.includes(user)
  return typeof content.body === 'string' && content.body.includes(user)
}
