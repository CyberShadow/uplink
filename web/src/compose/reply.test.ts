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
})
