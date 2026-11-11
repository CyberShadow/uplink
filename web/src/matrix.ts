// What Matrix events and sends say, as the views show it (SPEC §9).

import type { MatrixContent } from './generated/protocol'

/** A message to send, in Markdown. */
export function text(
  body: string,
  options: { emote?: boolean; reply_to?: string | null; replaces?: string | null } = {},
): MatrixContent {
  return {
    type: 'text',
    body,
    emote: options.emote ?? false,
    reply_to: options.reply_to ?? null,
    replaces: options.replaces ?? null,
  }
}

/** What a send sends, in a few words. */
export function sendSummary(content: MatrixContent): string {
  switch (content.type) {
    case 'text':
      return content.replaces !== null ? `Edit: ${content.body}` : content.body
    case 'reaction':
      return `Reaction ${content.key}`
    case 'redaction':
      return 'Deletion'
    case 'file':
      return content.name
  }
}
