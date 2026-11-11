import { describe, expect, it } from 'vitest'
import { links, permalink } from './matrix'

describe('links', () => {
  it('of HTML, are its links, not what the text says', () => {
    // As the Slack bridge sends them.
    const body = 'Maybe the main <http://dlang.org|dlang.org> site?'
    const html = 'Maybe the main <a href="http://dlang.org">dlang.org</a> site? <a href="mailto:a@b.c">mail</a>'
    expect(links(body, html)).toEqual(['http://dlang.org'])
  })

  it('of text, are its URLs', () => {
    expect(links('See https://example.org/a, and http://example.com.', null)).toEqual([
      'https://example.org/a',
      'http://example.com',
    ])
  })
})

describe('permalink', () => {
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
