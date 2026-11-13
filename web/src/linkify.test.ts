import { describe, expect, it } from 'vitest'
import { linkify, linkifyHtml } from './linkify'

describe('linkify', () => {
  it('links web addresses, with their scheme or without, and email addresses', () => {
    expect(linkify('See www.example.org/a(b), dlang.org! Or mail bob@example.com.')).toEqual([
      { text: 'See ', href: null },
      { text: 'www.example.org/a(b)', href: 'https://www.example.org/a(b)' },
      { text: ', ', href: null },
      { text: 'dlang.org', href: 'https://dlang.org' },
      { text: '! Or mail ', href: null },
      { text: 'bob@example.com', href: 'mailto:bob@example.com' },
      { text: '.', href: null },
    ])
  })

  it('leaves out the punctuation around a link', () => {
    expect(linkify('(<https://example.org/x?a=1>).')).toEqual([
      { text: '(<', href: null },
      { text: 'https://example.org/x?a=1', href: 'https://example.org/x?a=1' },
      { text: '>).', href: null },
    ])
  })

  it('links only the schemes of the web and of mail', () => {
    expect(linkify('file:///etc/passwd')).toEqual([{ text: 'file:///etc/passwd', href: null }])
    expect(linkify('ftp://x.org mailto:a@b.org').map((part) => part.href)).toEqual(['ftp://x.org', null, 'mailto:a@b.org'])
  })

  it('of text with no link, is the text', () => {
    expect(linkify('Hi there')).toEqual([{ text: 'Hi there', href: null }])
    expect(linkify('')).toEqual([])
  })
})

describe('linkifyHtml', () => {
  it('links text, not that of links or code', () => {
    const html = '<b>See dlang.org</b> <a href="https://x.org">y.org</a> <code>z.org</code> <pre>w.org</pre> v.org'
    expect(linkifyHtml(html, linkify)).toBe(
      '<b>See <a href="https://dlang.org" target="_blank" rel="noopener noreferrer">dlang.org</a></b> ' +
        '<a href="https://x.org">y.org</a> <code>z.org</code> <pre>w.org</pre> ' +
        '<a href="https://v.org" target="_blank" rel="noopener noreferrer">v.org</a>',
    )
  })

  it('escapes the text it links', () => {
    expect(linkifyHtml('&lt;b&gt; x.org', linkify)).toBe(
      '&lt;b&gt; <a href="https://x.org" target="_blank" rel="noopener noreferrer">x.org</a>',
    )
  })
})
