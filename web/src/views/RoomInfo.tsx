// A room's panel beside its timeline (SPEC §12.11), as Element's right
// panel: its name and topic, to change if the account may, inviting, and
// its members by membership and power, to remove, ban or unban; or one
// member's profile.

import { useEffect, useRef, useState } from 'preact/hooks'
import type { MemberAction, MemberInfo, Room, RoomPowers } from '../generated/protocol'
import { useServices } from '../context'
import { keepFocus } from '../keymap'
import { MEMBER_ACTIONS, displayName, roomLink, senderColor } from '../matrix'
import styles from './RoomInfo.module.css'
import { MemberAvatar } from './RoomEvent'
import { useOpenJoined } from './RoomStarter'

export function RoomInfo(props: { room: Room; close: () => void; openMember: (user: string) => void }) {
  const { room } = props
  const { matrix } = useServices()
  const members = matrix.members(room.id)
  const state = members.state.value
  const joined = room.membership === 'joined'
  return (
    <aside class={styles.panel} aria-label="Room info">
      <header class={styles.header}>
        <h2>Room info</h2>
        <button type="button" onMouseDown={keepFocus} onClick={props.close} aria-label="Close room info">
          ×
        </button>
      </header>
      {members.error.value !== null && (
        <p class={styles.error} role="alert">
          {members.error.value}
        </p>
      )}
      {state === null ? (
        <p class={styles.muted}>Loading…</p>
      ) : (
        <>
          <Settings room={room} powers={joined ? state.powers : null} />
          <Address room={room} />
          {joined && state.powers.invite && <Invite room={room} />}
          <MemberList room={room} members={state.members} open={props.openMember} />
        </>
      )}
    </aside>
  )
}

/** The room's name and topic: a form if the account may change either. */
function Settings(props: { room: Room; powers: RoomPowers | null }) {
  const { room, powers } = props
  const { matrix } = useServices()
  const [name, setName] = useState(room.name)
  const [topic, setTopic] = useState(room.topic ?? '')
  if (!powers?.name && !powers?.topic) {
    return room.topic === null ? null : <p class={styles.topic}>{room.topic}</p>
  }
  const changed = (name !== room.name && powers.name) || (topic !== (room.topic ?? '') && powers.topic)
  return (
    <form
      class={styles.settings}
      onSubmit={async (event) => {
        event.preventDefault()
        if (powers.name && name !== room.name) await matrix.send(room.id, { type: 'name', name: name.trim() })
        if (powers.topic && topic !== (room.topic ?? ''))
          await matrix.send(room.id, { type: 'topic', topic: topic.trim() })
      }}
    >
      <label>
        Name
        <input value={name} disabled={!powers.name} onInput={(e) => setName(e.currentTarget.value)} />
      </label>
      <label>
        Topic
        <textarea rows={3} value={topic} disabled={!powers.topic} onInput={(e) => setTopic(e.currentTarget.value)} />
      </label>
      <div class={styles.actions}>
        <button type="submit" disabled={!changed}>
          Save
        </button>
      </div>
    </form>
  )
}

/** The room's address and id, and a link to it, to copy. */
function Address(props: { room: Room }) {
  const { room } = props
  const { matrix } = useServices()
  const me = matrix.accounts.value!.find((a) => a.id === room.account)!.mxid
  return (
    <dl class={styles.address}>
      {room.alias !== null && <Copyable term="Address" value={room.alias} />}
      <Copyable term="Room ID" value={room.room_id} />
      <Copyable term="Link" value={roomLink(room, me)} />
    </dl>
  )
}

function Copyable(props: { term: string; value: string }) {
  const [copied, setCopied] = useState(false)
  return (
    <>
      <dt>{props.term}</dt>
      <dd>
        <code>{props.value}</code>
        <button
          type="button"
          class={styles.small}
          aria-label={`Copy ${props.term.toLowerCase()}`}
          onClick={async () => {
            await navigator.clipboard.writeText(props.value)
            setCopied(true)
          }}
        >
          {copied ? 'Copied' : 'Copy'}
        </button>
      </dd>
    </>
  )
}

function Invite(props: { room: Room }) {
  const { matrix } = useServices()
  const [user, setUser] = useState('')
  const valid = /^@[^:\s]+:\S+$/.test(user.trim())
  return (
    <form
      class={styles.invite}
      onSubmit={async (event) => {
        event.preventDefault()
        if (
          await matrix.send(props.room.id, {
            type: 'member',
            user: user.trim(),
            action: 'invite',
            reason: null,
          })
        ) {
          setUser('')
        }
      }}
    >
      <input
        aria-label="User to invite"
        placeholder="@user:example.org"
        value={user}
        onInput={(e) => setUser(e.currentTarget.value)}
      />
      <button type="submit" disabled={!valid}>
        Invite
      </button>
    </form>
  )
}

/** The members, as Element lists them: joined, by power, then by name;
 * then those invited, asking to join, and banned. Those who left are not
 * listed. */
function MemberList(props: { room: Room; members: readonly MemberInfo[]; open: (user: string) => void }) {
  const groups: [string, MemberInfo[]][] = [
    ['Joined', []],
    ['Invited', []],
    ['Asking to join', []],
    ['Banned', []],
  ]
  const group = { joined: 0, invited: 1, knocking: 2, banned: 3 } as const
  for (const member of props.members) {
    if (member.membership !== 'left') groups[group[member.membership]][1].push(member)
  }
  for (const [, members] of groups) {
    members.sort(
      (a, b) =>
        power(b) - power(a) ||
        displayName(a.user, a.name).localeCompare(displayName(b.user, b.name)) ||
        a.user.localeCompare(b.user),
    )
  }
  return (
    <>
      {groups
        .filter(([, members]) => members.length > 0)
        .map(([title, members]) => (
          <section key={title} aria-label={title}>
            <h3>
              {title} <span class={styles.muted}>{members.length}</span>
            </h3>
            <ul class={styles.members}>
              {members.map((member) => (
                <Member key={member.user} room={props.room} member={member} open={() => props.open(member.user)} />
              ))}
            </ul>
          </section>
        ))}
    </>
  )
}

/** Of a creator of a room of version 12, above any. */
function power(member: MemberInfo): number {
  return member.power ?? Infinity
}

/** A power level's role, as Element names them. */
function role(member: MemberInfo): string | null {
  if (member.power === null) return 'Owner'
  if (member.power >= 100) return 'Admin'
  if (member.power >= 50) return 'Moderator'
  return member.power > 0 ? `Power ${member.power}` : null
}

function Member(props: { room: Room; member: MemberInfo; open: () => void }) {
  const { room, member } = props
  const name = displayName(member.user, member.name)
  const roleName = role(member)
  return (
    <li class={styles.member} aria-label={name}>
      <button type="button" class={styles.line} onClick={props.open} title={`${name}'s profile`}>
        <MemberAvatar room={room} user={member.user} name={name} class={styles.avatar} />
        <span class={styles.who}>
          <span class={styles.name} style={{ color: senderColor(member.user) }}>
            {name}
          </span>
          <span class={styles.user}>{member.user}</span>
        </span>
        {roleName !== null && <span class={styles.role}>{roleName}</span>}
      </button>
    </li>
  )
}

const MEMBERSHIP: Record<MemberInfo['membership'], string | null> = {
  joined: null,
  invited: 'Invited',
  knocking: 'Asking to join',
  left: 'Left the room',
  banned: 'Banned',
}

/** A member's profile, as Element's user info: their avatar, name, id
 * and role in the room; a direct chat with them, mentioning them, and
 * removing or banning them if the account may. `back` returns to the
 * room's info, if it was opened from there. */
export function MemberPanel(props: {
  room: Room
  user: string
  back: (() => void) | null
  close: () => void
  /** Puts a mention of them in the room's box. */
  mention: (user: string, name: string) => void
}) {
  const { room, user } = props
  const { matrix } = useServices()
  const members = matrix.members(room.id)
  const member = members.state.value?.members.find((m) => m.user === user) ?? null
  const me = matrix.accounts.value!.find((a) => a.id === room.account)!.mxid
  const name = displayName(user, member?.name ?? null)
  const roleName = member && role(member)
  const membership = member ? MEMBERSHIP[member.membership] : members.state.value && 'Not a member'
  return (
    <aside class={styles.panel} aria-label="Member">
      <header class={styles.header}>
        {props.back && (
          <button type="button" onMouseDown={keepFocus} onClick={props.back} aria-label="Back to room info">
            ‹
          </button>
        )}
        <h2>Member</h2>
        <button type="button" onMouseDown={keepFocus} onClick={props.close} aria-label="Close member">
          ×
        </button>
      </header>
      {members.error.value !== null && (
        <p class={styles.error} role="alert">
          {members.error.value}
        </p>
      )}
      <div class={styles.profile}>
        <MemberAvatar room={room} user={user} name={name} class={styles.big} />
        <h3 style={{ color: senderColor(user) }}>{name}</h3>
        <span class={styles.user}>{user}</span>
        {(roleName || membership) && (
          <span class={styles.role}>{[roleName, membership].filter(Boolean).join(' · ')}</span>
        )}
      </div>
      {user !== me && (
        <>
          <DirectChat room={room} user={user} />
          {room.membership === 'joined' && (
            <button type="button" onClick={() => props.mention(user, name)}>
              Mention
            </button>
          )}
        </>
      )}
      {member && room.membership === 'joined' && <MemberActions room={room} member={member} />}
    </aside>
  )
}

/** Opens the account's direct chat with the user, starting one if there
 * is none. */
function DirectChat(props: { room: Room; user: string }) {
  const { matrix } = useServices()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [, setJoined] = useOpenJoined(props.room.account, noop)
  return (
    <>
      <button
        type="button"
        disabled={busy}
        onClick={async () => {
          setBusy(true)
          setError(null)
          try {
            setJoined(await matrix.join({ type: 'start_direct_chat', account: props.room.account, user: props.user }))
          } catch (e) {
            setError((e as Error).message)
          }
          setBusy(false)
        }}
      >
        Message
      </button>
      {error !== null && (
        <p class={styles.error} role="alert">
          {error}
        </p>
      )}
    </>
  )
}

function noop() {}

/** Removing (revoking an invitation), banning or unbanning the member,
 * as the account may, each asking a reason first. */
function MemberActions(props: { room: Room; member: MemberInfo }) {
  const { room, member } = props
  const { matrix } = useServices()
  // The action being asked a reason for.
  const [acting, setActing] = useState<MemberAction | null>(null)
  const actions: MemberAction[] =
    member.membership === 'banned'
      ? member.kickable && member.bannable
        ? ['unban']
        : []
      : member.membership === 'left'
      ? member.bannable
        ? ['ban']
        : []
      : [...(member.kickable ? (['kick'] as const) : []), ...(member.bannable ? (['ban'] as const) : [])]
  const label = (action: MemberAction) =>
    action === 'kick' && member.membership === 'invited' ? 'Revoke invitation' : MEMBER_ACTIONS[action]
  if (acting !== null) {
    return (
      <Reason
        label={label(acting)}
        cancel={() => setActing(null)}
        submit={async (reason) => {
          const sent = await matrix.send(room.id, {
            type: 'member',
            user: member.user,
            action: acting,
            reason,
          })
          if (sent) setActing(null)
        }}
      />
    )
  }
  return (
    <>
      {actions.map((action) => (
        <button key={action} type="button" class={styles.danger} onClick={() => setActing(action)}>
          {label(action)}
        </button>
      ))}
    </>
  )
}

/** Asks the reason of a change to a member, which it then makes. */
function Reason(props: { label: string; cancel: () => void; submit: (reason: string | null) => void }) {
  const [reason, setReason] = useState('')
  const input = useRef<HTMLInputElement>(null)
  useEffect(() => input.current?.focus(), [])
  return (
    <form
      class={styles.reason}
      onSubmit={(event) => {
        event.preventDefault()
        props.submit(reason.trim() || null)
      }}
    >
      <input
        ref={input}
        aria-label="Reason"
        placeholder="Reason (optional)"
        value={reason}
        onInput={(e) => setReason(e.currentTarget.value)}
        onKeyDown={(e) => e.key === 'Escape' && props.cancel()}
      />
      <button type="submit">{props.label}</button>
      <button type="button" onClick={props.cancel}>
        Cancel
      </button>
    </form>
  )
}
