import { describe, expect, it } from 'vitest'
import { links } from './matrix'

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
