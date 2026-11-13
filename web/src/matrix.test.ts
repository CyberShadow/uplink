import { describe, expect, it } from 'vitest'
import { gist, linkify, links, mxcPath, permalink, roomLink } from './matrix'
import type { RoomPreview } from './generated/protocol'

describe('gist', () => {
  const said = (fields: Partial<RoomPreview>): RoomPreview => ({
    event: 1,
    sender: '@bob:x',
    sender_name: 'Bob',
    kind: 'm.room.message',
    msgtype: 'm.text',
    body: 'Hi',
    ...fields,
  })
  it('says who said what, the account as You, media and emotes in words', () => {
    expect(gist(said({}), '@me:x')).toBe('Bob: Hi')
    expect(gist(said({ sender: '@me:x' }), '@me:x')).toBe('You: Hi')
    expect(gist(said({ sender_name: null }), '@me:x')).toBe('bob: Hi')
    expect(gist(said({ msgtype: 'm.image', body: 'cat.png' }), '@me:x')).toBe('Bob sent an image')
    expect(gist(said({ msgtype: 'm.emote', body: 'waves' }), '@me:x')).toBe('* Bob waves')
    expect(gist(said({ kind: 'm.room.encrypted', msgtype: null, body: null }), '@me:x')).toBe('Bob: Unable to decrypt')
  })
})

describe('links', () => {
  it('of HTML, are its links, not what the text says', () => {
    // As the Slack bridge sends them.
    const body = 'Maybe the main <http://dlang.org|dlang.org> site?'
    const html = 'Maybe the main <a href="http://dlang.org">dlang.org</a> site? <a href="mailto:a@b.c">mail</a>'
    expect(links(body, html)).toEqual(['http://dlang.org'])
  })

  it('of text, are its web addresses', () => {
    expect(links('See https://example.org/a, and www.example.com; mail a@b.org.', null)).toEqual([
      'https://example.org/a',
      'https://www.example.com',
    ])
  })

  it('of HTML, are also the web addresses of its text', () => {
    expect(links('See *www.dlang.org*', 'See <b>www.dlang.org</b>')).toEqual(['https://www.dlang.org'])
  })
})

describe('linkify', () => {
  it("links users' and rooms' addresses to them, not their servers", () => {
    expect(linkify('Ask @alice:example.org, in #room:matrix.org:8448.')).toEqual([
      { text: 'Ask ', href: null },
      { text: '@alice:example.org', href: 'https://matrix.to/#/%40alice%3Aexample.org' },
      { text: ', in ', href: null },
      { text: '#room:matrix.org:8448', href: 'https://matrix.to/#/%23room%3Amatrix.org%3A8448' },
      { text: '.', href: null },
    ])
  })

  it('links addresses only as words of their own', () => {
    const url = 'https://matrix.to/#/@alice:example.org'
    expect(linkify(url)).toEqual([{ text: url, href: url }])
    expect(linkify('@bob: hi')).toEqual([{ text: '@bob: hi', href: null }])
    expect(linkify('bob@example.com')).toEqual([{ text: 'bob@example.com', href: 'mailto:bob@example.com' }])
  })
})

describe('permalink', () => {
  it('links to a room by its address, or by its id by way of the account\'s server', () => {
    expect(roomLink({ room_id: '!abc:example.org', alias: '#hall:example.org' }, '@me:home.org')).toBe(
      'https://matrix.to/#/%23hall%3Aexample.org',
    )
    const link = roomLink({ room_id: '!abc:example.org', alias: null }, '@me:home.org')
    expect(link).toBe('https://matrix.to/#/!abc%3Aexample.org?via=home.org')
    expect(permalink(link)).toEqual({ type: 'room', id: '!abc:example.org' })
  })

  it('is what a matrix.to link names: a user, or a room', () => {
    expect(permalink('https://matrix.to/#/@bob:example.org')).toEqual({ type: 'user', id: '@bob:example.org' })
    expect(permalink('https://matrix.to/#/%40bob%3Aexample.org')).toEqual({ type: 'user', id: '@bob:example.org' })
    expect(permalink('https://matrix.to/#/!abc:example.org?via=example.org')).toEqual({ type: 'room', id: '!abc:example.org' })
    expect(permalink('https://matrix.to/#/#hall:example.org')).toEqual({ type: 'room', id: '#hall:example.org' })
  })

  it('of other links, and of events, is none', () => {
    expect(permalink('https://matrix.to/#/!abc:example.org/$event')).toBeNull()
    expect(permalink('https://matrix.to/#/nobody')).toBeNull()
    expect(permalink('https://matrix.to/#/%E0%A4%A')).toBeNull()
    expect(permalink('https://example.org/#/@bob:example.org')).toBeNull()
  })
})

describe('mxcPath', () => {
  it("is a media's server and id, as the browser takes them as a path", () => {
    expect(mxcPath('mxc://example.org/abc_D-1')).toBe('example.org/abc_D-1')
    expect(mxcPath('mxc://example.org:8448/a')).toBe('example.org:8448/a')
    expect(mxcPath('mxc://[::1]:8448/a')).toBe('[::1]:8448/a')
  })

  it('of anything a browser would resolve, out of the media, or read otherwise, is none', () => {
    for (const mxc of [
      'mxc://../..',
      'mxc://./a',
      'mxc://x/..',
      'mxc://x/%2e%2e',
      'mxc://x/a/../..',
      'mxc://x/a?url=b',
      'mxc://x/a#b',
      'mxc://x\\..\\../a',
      'mxc://x/a\tb',
      'mxc:///a',
      'mxc://x/',
      'https://x/a',
    ]) {
      expect(mxcPath(mxc), mxc).toBeNull()
    }
  })
})
