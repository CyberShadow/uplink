import { describe, expect, it } from 'vitest'
import type { Identity, MessageContent } from '../generated/protocol'
import { respond } from './reply'

const own: Identity[] = [
  { address: 'me@example.org', name: 'Me' },
  { address: 'me@other.example', name: null },
  { domain: 'example.com', name: null },
]

function message(fields: Partial<MessageContent>): MessageContent {
  return {
    id: 1,
    message_id: '1@x',
    subject: 'Plans',
    from_addr: 'alice@example.net',
    from_name: 'Alice',
    to_addrs: [],
    cc_addrs: [],
    original_to: [],
    delivered_to: [],
    date_sent: 0,
    body_text: 'Hi',
    body_html: null,
    remote_images: [],
    tracking_pixels: 0,
    image_rules: [],
    attachments: [],
    ...fields,
  }
}

describe('replies', () => {
  it('to all leave out every address of the account', () => {
    const original = message({
      to_addrs: ['ME@example.org', 'bob@example.net'],
      cc_addrs: ['me@other.example', 'shop@example.com', 'carol@example.net', 'bob@example.net'],
    })
    const { content } = respond('replyAll', original, own)
    expect(content.to).toBe('Alice <alice@example.net>')
    expect(content.cc).toBe('bob@example.net, carol@example.net')
  })

  it('are from the address the original was sent to', () => {
    const from = (fields: Partial<MessageContent>) => respond('reply', message(fields), own).content.from
    // Its envelope's, before forwarding rewrote it.
    expect(
      from({ original_to: ['Shop@Example.com'], delivered_to: ['me@other.example'], to_addrs: ['me@other.example'] }),
    ).toBe('Shop@Example.com')
    // A delivery to one of the account's, but its own (where every copy
    // is delivered) and forwarding's rewritten addresses.
    expect(
      from({ delivered_to: ['me@example.org', 'me+fwd@example.org', 'list@example.com'], to_addrs: ['me@other.example'] }),
    ).toBe('list@example.com')
    expect(from({ delivered_to: ['me@example.org', 'me+fwd@example.org'], to_addrs: ['me@other.example'] })).toBe(
      'me@other.example',
    )
    // To, then Cc.
    expect(from({ to_addrs: ['bob@example.net'], cc_addrs: ['bob@example.net', 'x@example.com'] })).toBe('x@example.com')
    // The user's own mail, followed up.
    expect(from({ from_addr: 'me@other.example', to_addrs: ['bob@example.net'] })).toBe('me@other.example')
    // None, or the account's own: its own address, the default.
    expect(from({ to_addrs: ['bob@example.net'] })).toBe('')
    expect(from({ to_addrs: ['ME@example.org'], cc_addrs: ['x@example.com'] })).toBe('')
    // Forwards too.
    expect(respond('forward', message({ to_addrs: ['x@example.com'] }), own).content.from).toBe('x@example.com')
  })
})
