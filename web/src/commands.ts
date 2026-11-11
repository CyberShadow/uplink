// The composer's commands (SPEC §12.11), as Element's: a message
// beginning with `/` is one, unless with `//`, which sends it with one
// `/`.

import type { MemberAction } from './generated/protocol'

/** What a command does. */
export type Command =
  | { type: 'message'; body: string; markdown: boolean; emote: boolean }
  | { type: 'member'; action: MemberAction; user: string; reason: string | null }
  | { type: 'name'; name: string }
  | { type: 'topic'; topic: string }
  | { type: 'join'; room: string }
  | { type: 'leave' }

export interface CommandHelp {
  name: string
  args: string
  description: string
}

/** Plain text a command puts before the message, as Element's do. */
const PREFIXES: Record<string, string> = {
  shrug: '¯\\_(ツ)_/¯',
  tableflip: '(╯°□°）╯︵ ┻━┻',
  unflip: '┬──┬ ノ( ゜-゜ノ)',
  lenny: '( ͡° ͜ʖ ͡°)',
}

export const COMMANDS: readonly CommandHelp[] = [
  { name: 'me', args: '<message>', description: 'Sends it as an action' },
  { name: 'plain', args: '<message>', description: 'Sends it as plain text, without Markdown' },
  { name: 'shrug', args: '[<message>]', description: `Puts ${PREFIXES.shrug} before it` },
  { name: 'tableflip', args: '[<message>]', description: `Puts ${PREFIXES.tableflip} before it` },
  { name: 'unflip', args: '[<message>]', description: `Puts ${PREFIXES.unflip} before it` },
  { name: 'lenny', args: '[<message>]', description: `Puts ${PREFIXES.lenny} before it` },
  { name: 'invite', args: '<user-id>', description: 'Invites the user to the room' },
  { name: 'remove', args: '<user-id> [<reason>]', description: 'Removes the user from the room' },
  { name: 'ban', args: '<user-id> [<reason>]', description: 'Bans the user from the room' },
  { name: 'unban', args: '<user-id> [<reason>]', description: 'Unbans the user' },
  { name: 'topic', args: '<topic>', description: "Sets the room's topic" },
  { name: 'roomname', args: '<name>', description: 'Names the room' },
  { name: 'join', args: '<address>', description: 'Joins the room at the address' },
  { name: 'leave', args: '', description: 'Leaves the room' },
]

/** Element's other names for some. */
const ALIASES: Record<string, string> = { kick: 'remove', part: 'leave', j: 'join' }

/** What `text` asks for: a message, or a command; or why it is not one
 * uplink knows. */
export function parse(text: string): Command | { type: 'error'; message: string } {
  if (text.startsWith('//')) return { type: 'message', body: text.slice(1), markdown: true, emote: false }
  if (!text.startsWith('/')) return { type: 'message', body: text, markdown: true, emote: false }
  const match = /^\/(\S+)(?:\s+([\s\S]*))?$/.exec(text)
  if (!match) return { type: 'error', message: 'Type a command after the /. To send a /, begin with //.' }
  const name = ALIASES[match[1]] ?? match[1]
  const rest = (match[2] ?? '').trim()
  const usage = (): { type: 'error'; message: string } => {
    const help = COMMANDS.find((c) => c.name === name)!
    return { type: 'error', message: `Usage: /${name} ${help.args}` }
  }
  // A user id, and what follows it.
  const user = (): [string, string | null] | null => {
    const [id, ...reason] = rest.split(/\s+/)
    return /^@[^:\s]+:\S+$/.test(id) ? [id, reason.join(' ') || null] : null
  }
  switch (name) {
    case 'me':
      return rest === '' ? usage() : { type: 'message', body: rest, markdown: true, emote: true }
    case 'plain':
      return rest === '' ? usage() : { type: 'message', body: rest, markdown: false, emote: false }
    case 'shrug':
    case 'tableflip':
    case 'unflip':
    case 'lenny':
      return {
        type: 'message',
        body: rest === '' ? PREFIXES[name] : `${PREFIXES[name]} ${rest}`,
        markdown: false,
        emote: false,
      }
    case 'invite': {
      const target = user()
      if (!target || target[1] !== null) return usage()
      return { type: 'member', action: 'invite', user: target[0], reason: null }
    }
    case 'remove':
    case 'ban':
    case 'unban': {
      const target = user()
      if (!target) return usage()
      const action = name === 'remove' ? 'kick' : name
      return { type: 'member', action, user: target[0], reason: target[1] }
    }
    case 'topic':
      return rest === '' ? usage() : { type: 'topic', topic: rest }
    case 'roomname':
      return rest === '' ? usage() : { type: 'name', name: rest }
    case 'join':
      return /^[#!]\S+$/.test(rest) ? { type: 'join', room: rest } : usage()
    case 'leave':
      return rest === '' ? { type: 'leave' } : usage()
    default:
      return { type: 'error', message: `Unknown command /${match[1]}. To send it as a message, begin with //.` }
  }
}
