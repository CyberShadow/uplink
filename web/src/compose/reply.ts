// Drafts replying to or forwarding a message.

import type { DraftContent, Identity, MessageContent, MessageId } from '../generated/protocol'
import { dateTime } from '../format'
import { isOwn } from '../identities'

export type Response = 'reply' | 'replyAll' | 'forward'

export interface NewDraft {
  content: DraftContent
  inReplyTo: MessageId | null
}

export const EMPTY: DraftContent = { from: '', to: '', cc: '', bcc: '', subject: '', body: '' }

/** A draft responding to `message`, from the account of identities
 * `own`. */
export function respond(kind: Response, message: MessageContent, own: readonly Identity[]): NewDraft {
  const subject = message.subject ?? ''
  const sender = message.from_addr ? mailbox(message.from_name, message.from_addr) : ''
  const date = dateTime((message.date_sent ?? 0) * 1000)
  if (kind === 'forward') {
    const header = [
      '---------- Forwarded message ---------',
      `From: ${sender}`,
      `Date: ${date}`,
      `Subject: ${subject}`,
      `To: ${message.to_addrs.join(', ')}`,
    ]
    return {
      content: {
        ...EMPTY,
        subject: prefixed('Fwd', subject),
        body: `<p></p><p>${header.map(escape).join('<br>')}</p>${paragraphs(message.body_text)}`,
      },
      inReplyTo: null,
    }
  }
  const others =
    kind === 'replyAll'
      ? [...message.to_addrs, ...message.cc_addrs].filter(
          (addr, i, all) => !isOwn(own, addr) && addr !== message.from_addr && all.indexOf(addr) === i,
        )
      : []
  return {
    content: {
      ...EMPTY,
      to: sender,
      cc: others.join(', '),
      subject: prefixed('Re', subject),
      body: `<p></p><p>${escape(`On ${date}, ${sender} wrote:`)}</p><blockquote>${paragraphs(message.body_text)}</blockquote>`,
    },
    inReplyTo: message.id,
  }
}

function prefixed(prefix: string, subject: string): string {
  return new RegExp(`^${prefix}:`, 'i').test(subject) ? subject : `${prefix}: ${subject}`
}

/** An address as typed in an address field. */
function mailbox(name: string | null, addr: string): string {
  return name ? `${/[",<>@]/.test(name) ? JSON.stringify(name) : name} <${addr}>` : addr
}

function paragraphs(text: string): string {
  return text
    .split(/\n\s*\n/)
    .map((p) => `<p>${escape(p.trim()).replaceAll('\n', '<br>')}</p>`)
    .join('')
}

function escape(text: string): string {
  return text.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;')
}
