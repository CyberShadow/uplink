import { describe, expect, it } from 'vitest'
import { parse } from './commands'
import { byShortcode, loadEmoji, searchEmoji } from './emoji'
import { completing } from './views/Autocomplete'

describe('commands', () => {
  it('sends messages, `//` escaping a leading /', () => {
    expect(parse('hi')).toEqual({ type: 'message', body: 'hi', markdown: true, emote: false })
    expect(parse('//etc/hosts')).toEqual({ type: 'message', body: '/etc/hosts', markdown: true, emote: false })
    expect(parse('/me waves')).toEqual({ type: 'message', body: 'waves', markdown: true, emote: true })
    expect(parse('/plain **as is**')).toEqual({ type: 'message', body: '**as is**', markdown: false, emote: false })
    expect(parse('/shrug fine')).toEqual({ type: 'message', body: '¯\\_(ツ)_/¯ fine', markdown: false, emote: false })
    expect(parse('/lenny')).toEqual({ type: 'message', body: '( ͡° ͜ʖ ͡°)', markdown: false, emote: false })
  })

  it('changes members, the room, and joins and leaves', () => {
    expect(parse('/invite @bob:example.org')).toEqual({
      type: 'member',
      action: 'invite',
      user: '@bob:example.org',
      reason: null,
    })
    expect(parse('/kick @bob:example.org too loud')).toEqual({
      type: 'member',
      action: 'kick',
      user: '@bob:example.org',
      reason: 'too loud',
    })
    expect(parse('/unban @bob:example.org')).toMatchObject({ action: 'unban', reason: null })
    expect(parse('/topic All things\nRust')).toEqual({ type: 'topic', topic: 'All things\nRust' })
    expect(parse('/roomname Lounge')).toEqual({ type: 'name', name: 'Lounge' })
    expect(parse('/join #rust:example.org')).toEqual({ type: 'join', room: '#rust:example.org' })
    expect(parse('/part')).toEqual({ type: 'leave' })
  })

  it('says what is wrong with one', () => {
    expect(parse('/invite bob')).toEqual({ type: 'error', message: 'Usage: /invite <user-id>' })
    expect(parse('/invite @bob:example.org why')).toEqual({ type: 'error', message: 'Usage: /invite <user-id>' })
    expect(parse('/me')).toEqual({ type: 'error', message: 'Usage: /me <message>' })
    expect(parse('/join lounge')).toEqual({ type: 'error', message: 'Usage: /join <address>' })
    expect(parse('/frobnicate')).toEqual({
      type: 'error',
      message: 'Unknown command /frobnicate. To send it as a message, begin with //.',
    })
  })
})

describe('completions', () => {
  it('complete commands, mentions and emoji where typed', () => {
    expect(completing('/inv', 4)).toEqual({ kind: 'command', query: 'inv', start: 0 })
    expect(completing('/invite @b', 10)).toEqual({ kind: 'mention', query: 'b', start: 8 })
    expect(completing('hi @', 4)).toEqual({ kind: 'mention', query: '', start: 3 })
    expect(completing('mail@example', 12)).toBeNull()
    expect(completing('ok :th', 6)).toEqual({ kind: 'emoji', query: 'th', start: 3 })
    expect(completing('ok :t', 5)).toBeNull()
    expect(completing('12:30', 5)).toBeNull()
    // Up to the caret only.
    expect(completing('hi @bo there', 6)).toEqual({ kind: 'mention', query: 'bo', start: 3 })
  })

  it('find emoji by shortcode, those beginning with it first', async () => {
    const emoji = await loadEmoji()
    expect(byShortcode(emoji, '+1')?.unicode).toBe('👍')
    expect(searchEmoji(emoji, 'thumbs').map((e) => e.unicode)).toEqual(['👍', '👎'])
    expect(searchEmoji(emoji, 'smile')[0].unicode).toBe('😄')
  })
})
