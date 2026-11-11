// A room's panel beside its timeline (SPEC §12.11), as Element's right
// panel: its name and topic, to change if the account may, inviting, and
// its members by membership and power, to remove, ban or unban.

import { useEffect, useRef, useState } from 'preact/hooks'
import type { MemberAction, MemberInfo, Room, RoomPowers } from '../generated/protocol'
import { useServices } from '../context'
import { keepFocus } from '../keymap'
import { MEMBER_ACTIONS, displayName, senderColor } from '../matrix'
import styles from './RoomInfo.module.css'
import { MemberAvatar } from './RoomEvent'

export function RoomInfo(props: { room: Room; close: () => void }) {
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
          {joined && state.powers.invite && <Invite room={room} />}
          <MemberList room={room} members={state.members} joined={joined} />
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
function MemberList(props: { room: Room; members: readonly MemberInfo[]; joined: boolean }) {
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
                <Member key={member.user} room={props.room} member={member} joined={props.joined} />
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

function Member(props: { room: Room; member: MemberInfo; joined: boolean }) {
  const { room, member } = props
  const { matrix } = useServices()
  // The action being asked a reason for.
  const [acting, setActing] = useState<MemberAction | null>(null)
  const name = displayName(member.user, member.name)
  const actions: MemberAction[] = !props.joined
    ? []
    : member.membership === 'banned'
    ? member.kickable && member.bannable
      ? ['unban']
      : []
    : [...(member.kickable ? (['kick'] as const) : []), ...(member.bannable ? (['ban'] as const) : [])]
  const label = (action: MemberAction) =>
    action === 'kick' && member.membership === 'invited' ? 'Revoke invitation' : MEMBER_ACTIONS[action]
  const roleName = role(member)
  return (
    <li class={styles.member} aria-label={name}>
      <div class={styles.line}>
        <MemberAvatar room={room} user={member.user} name={name} class={styles.avatar} />
        <span class={styles.who}>
          <span class={styles.name} style={{ color: senderColor(member.user) }}>
            {name}
          </span>
          <span class={styles.user}>{member.user}</span>
        </span>
        {roleName !== null && <span class={styles.role}>{roleName}</span>}
        {acting === null &&
          actions.map((action) => (
            <button key={action} type="button" class={styles.small} onClick={() => setActing(action)}>
              {label(action)}
            </button>
          ))}
      </div>
      {acting !== null && (
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
      )}
    </li>
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
