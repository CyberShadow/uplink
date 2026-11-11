import { signal } from '@preact/signals'
import { fireEvent, render, screen, waitFor, within } from '@testing-library/preact'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { Entry, MemberState } from '../generated/protocol'
import { Client } from '../stores/client'
import { conversationKey } from '../stores/mail'
import { TIMELINE_PAGE } from '../stores/matrix'
import { scrolledTo } from '../test/intersection'
import { FakeServer, ME, RECOVERY_KEY } from '../test/server'
import { App } from './App'
import { text } from '../matrix'

let server: FakeServer
let client: Client

beforeEach(() => {
  server = new FakeServer()
  server.addAccount('work', ['Projects', 'Travel'])
  server.deliver('work', 'Third')
  server.deliver('work', 'Second')
  server.deliver('work', 'First')
  client = new Client(server)
  server.attach(client)
})

async function start(path = '/mail/work/INBOX') {
  history.replaceState(null, '', path)
  render(<App client={client} status={signal('open')} />)
  client.reset()
  if (path.startsWith('/rooms/')) await screen.findByRole('list', { name: 'Timeline' })
  else if (path === '/todos') await screen.findByRole('region', { name: 'To-dos' })
  else if (path === '/history') await screen.findByRole('list', { name: 'History' })
  else if (path === '/outbox') await screen.findByText(/^The outbox is empty/)
  else if (path.startsWith('/matrix/')) await screen.findByRole('heading', { name: /^Security of/ })
  else if (path === '/') await screen.findByRole('region', { name: 'Home' })
  else await screen.findByRole('list', { name: 'Messages' })
  return userEvent.setup()
}

function rows() {
  return within(screen.getByRole('list', { name: 'Messages' })).queryAllByRole('listitem')
}
const subjects = () => rows().map((row) => row.querySelector('[class*=subject]')!.textContent)
const focusedSubject = () =>
  rows()
    .find((row) => row.getAttribute('aria-current') === 'true')
    ?.querySelector('[class*=subject]')!.textContent

describe('mail', () => {
  it('shows how far an account\'s first sync has got', async () => {
    server.syncing.set('work', { stored: 4321, total: 10000 })
    server.addAccount('home')
    server.syncing.set('home', null)
    await start()
    const nav = screen.getByRole('navigation')
    expect(within(nav).getByTitle('4,321 of about 10,000 messages').textContent).toBe(' (syncing, 43%)')
    expect(within(nav).getByText('(syncing)')).toBeTruthy()
  })

  it('folds an account in the nav, and labels nested in a label', async () => {
    server.addAccount('home', ['Family', 'Family/Kids'])
    const user = await start()
    const nav = within(screen.getByRole('navigation'))
    const kids = () => nav.queryByRole('link', { name: 'Kids' })
    expect(kids()).toBeTruthy()
    await user.click(nav.getByRole('button', { name: 'Collapse Family' }))
    expect(kids()).toBeNull()
    await user.click(nav.getByRole('button', { name: 'Expand Family' }))
    expect(kids()).toBeTruthy()

    const home = nav.getByRole('button', { name: /home@example.com/ })
    await user.click(home)
    expect(home.getAttribute('aria-expanded')).toBe('false')
    expect(nav.queryByRole('link', { name: 'Family' })).toBeNull()
    expect(localStorage.getItem('uplink.nav.collapsed')).toBe('["mail:home"]')
    await user.click(home)
    expect(nav.getByRole('link', { name: 'Family' })).toBeTruthy()
  })

  it('opens the first Inbox, with labels in the nav', async () => {
    await start()
    expect(location.pathname).toBe('/mail/work/INBOX')
    expect(subjects()).toEqual(['First', 'Second', 'Third'])
    const nav = screen.getByRole('navigation')
    expect(within(nav).getByRole('link', { name: /Inbox/ }).getAttribute('aria-current')).toBe('page')
    expect(within(nav).getByRole('link', { name: 'Projects' })).toBeTruthy()
  })

  describe('conversations', () => {
    /** A thread of three in the Inbox: Alice's, the user's reply, and
     * Bob's; the reply not in the Inbox. */
    function thread() {
      const first = server.deliver('work', 'Plans', ['INBOX'], 't1')
      first.from_name = 'Alice Smith'
      first.from_addr = 'alice@example.org'
      const reply = server.deliver('work', 'Re: Plans', ['SENT'], 't1')
      reply.from_name = null
      reply.from_addr = 'work@example.com'
      const last = server.deliver('work', 'Re: Plans', ['INBOX', 'UNREAD'], 't1')
      return [first, reply, last]
    }

    it('lists a thread as one row, by its latest', async () => {
      thread()
      await start('/mail/work/INBOX')
      expect(subjects()).toEqual(['Re: Plans', 'First', 'Second', 'Third'])
      expect(rows()[0].querySelector('[class*=from]')!.textContent).toBe('Alice, me, Bob 3')
      expect(rows()[0].className).toMatch(/unread/)
    })

    it('stars the latest message, and unstars every one', async () => {
      const [first, reply, last] = thread()
      const starred = server.labelId('work', 'STARRED')
      first.labels.push(starred)
      reply.labels.push(starred)
      const user = await start('/mail/work/INBOX')
      await user.keyboard('s')
      await waitFor(() => expect(server.labelsOf(first.id)).not.toContain(starred))
      expect(server.labelsOf(reply.id)).not.toContain(starred)
      await user.keyboard('s')
      await waitFor(() => expect(server.labelsOf(last.id)).toContain(starred))
      expect(server.labelsOf(first.id)).not.toContain(starred)
    })

    it('archives every message, and marks the latest unread', async () => {
      const [first, reply, last] = thread()
      const unread = server.labelId('work', 'UNREAD')
      last.labels = last.labels.filter((l) => l !== unread)
      const user = await start('/mail/work/INBOX')
      await user.keyboard('{Shift>}u{/Shift}')
      await waitFor(() => expect(server.labelsOf(last.id)).toContain(unread))
      expect(server.labelsOf(first.id)).not.toContain(unread)
      await user.keyboard('e')
      await waitFor(() => expect(subjects()).toEqual(['First', 'Second', 'Third']))
      expect(server.lastDescription()).toBe('Changed 3 messages')
      expect(server.labelsOf(first.id)).not.toContain(server.labelId('work', 'INBOX'))
      expect(server.labelsOf(reply.id)).toEqual([server.labelId('work', 'SENT')])
    })

    it('opens with the latest and the unread expanded, reading them', async () => {
      const [first, , last] = thread()
      const unread = server.labelId('work', 'UNREAD')
      first.labels.push(unread)
      const user = await start('/mail/work/INBOX')
      await user.keyboard('{Enter}')
      const messages = within(await screen.findByRole('list', { name: 'Conversation' })).getAllByRole('listitem')
      expect(messages).toHaveLength(3)
      const expanded = () => messages.map((m) => m.querySelector('section')!.getAttribute('aria-expanded'))
      expect(expanded()).toEqual(['true', 'false', 'true'])
      await screen.findByText('Body of Plans')
      await waitFor(() => expect(server.labelsOf(last.id)).not.toContain(unread))
      expect(server.labelsOf(first.id)).not.toContain(unread)
      // Its header toggles one.
      await user.click(messages[1].querySelector('header')!)
      expect(expanded()).toEqual(['true', 'true', 'true'])
    })

    it('replies to the latest message', async () => {
      const [, , last] = thread()
      const user = await start('/mail/work/INBOX')
      await user.keyboard('{Enter}')
      await screen.findByRole('list', { name: 'Conversation' })
      await user.keyboard('r')
      await waitFor(() => expect(server.requests.some((r) => r.type === 'create_draft')).toBe(true))
      const draft = server.requests.find((r) => r.type === 'create_draft')!
      expect(draft).toMatchObject({ in_reply_to: last.message })
    })

    describe('actions by pointer', () => {
      const conversation = async () => {
        const [first, second, last] = thread()
        const user = await start('/mail/work/INBOX')
        await user.keyboard('{Enter}')
        const messages = within(await screen.findByRole('list', { name: 'Conversation' })).getAllByRole('listitem')
        await screen.findByText('Body of Re: Plans')
        return { user, messages, first, second, last }
      }
      const created = () => server.requests.filter((r) => r.type === 'create_draft')

      it('replies, to all, and forwards from the bar below the conversation, taking the latest', async () => {
        const { user, last } = await conversation()
        await user.click(screen.getByRole('button', { name: 'Reply to all' }))
        await waitFor(() => expect(created()).toHaveLength(1))
        expect(created()[0]).toMatchObject({ in_reply_to: last.message })
      })

      it("answers an earlier message from its own tools and menu", async () => {
        const { user, messages, first } = await conversation()
        await user.click(messages[0].querySelector('header')!)
        await screen.findByText('Body of Plans')
        const tools = within(messages[0])
        await user.click(tools.getByRole('button', { name: 'Reply' }))
        await waitFor(() => expect(created()).toHaveLength(1))
        expect(created()[0]).toMatchObject({ in_reply_to: first.message })
      })

      it('deletes one message, and marks unread from one on, leaving the conversation', async () => {
        const { user, messages, first, second, last } = await conversation()
        const trash = server.labelId('work', 'TRASH')
        const unread = server.labelId('work', 'UNREAD')
        await user.click(within(messages[2]).getByRole('button', { name: 'More' }))
        expect(screen.getByRole('menu', { name: 'More' })).toBeTruthy()
        await user.click(screen.getByRole('menuitem', { name: 'Delete this message' }))
        await waitFor(() => expect(server.labelsOf(last.id)).toContain(trash))
        expect(server.labelsOf(first.id)).not.toContain(trash)
        expect(screen.queryByRole('menu')).toBeNull()
        // It leaves the view; the user's reply is the latest, expanded.
        const left = () => within(screen.getByRole('list', { name: 'Conversation' })).getAllByRole('listitem')
        await waitFor(() => expect(left()).toHaveLength(2))
        await user.click(await within(left()[1]).findByRole('button', { name: 'More' }))
        await user.click(screen.getByRole('menuitem', { name: 'Mark unread from here' }))
        await waitFor(() => expect(server.labelsOf(second.id)).toContain(unread))
        expect(server.labelsOf(first.id)).not.toContain(unread)
        await screen.findByRole('list', { name: 'Messages' })
      })

      it("shows a message's original, as its account has it", async () => {
        const { user, messages, last } = await conversation()
        const opened: unknown[][] = []
        const open = window.open
        window.open = (...args: unknown[]) => {
          opened.push(args)
          return null
        }
        try {
          await user.click(within(messages[2]).getByRole('button', { name: 'More' }))
          await user.click(screen.getByRole('menuitem', { name: 'Show original' }))
        } finally {
          window.open = open
        }
        expect(opened).toEqual([[`/api/entries/${last.id}/raw`, '_blank', 'noopener']])
      })

      it('closes the menu on Escape', async () => {
        const { user, messages } = await conversation()
        await user.click(within(messages[2]).getByRole('button', { name: 'More' }))
        await user.keyboard('{Escape}')
        expect(screen.queryByRole('menu')).toBeNull()
        expect(screen.getByRole('list', { name: 'Conversation' })).toBeTruthy()
      })
    })
  })

  it('keeps moving while j or k is held, but archives once while e is', async () => {
    server.deliver('work', 'Zeroth')
    await start('/mail/work/INBOX')
    const hold = (key: string, repeats: number) => {
      fireEvent.keyDown(document.body, { key })
      for (let i = 0; i < repeats; i++) fireEvent.keyDown(document.body, { key, repeat: true })
      fireEvent.keyUp(document.body, { key })
    }
    hold('j', 1)
    expect(focusedSubject()).toBe('Second')
    hold('k', 1)
    expect(focusedSubject()).toBe('Zeroth')
    hold('e', 2)
    await waitFor(() => expect(subjects()).toEqual(['First', 'Second', 'Third']))
  })

  it('moves with j and k, selects with x, and archives the selection with e', async () => {
    const user = await start('/mail/work/INBOX')
    expect(focusedSubject()).toBe('First')
    await user.keyboard('jx')
    expect(focusedSubject()).toBe('Second')
    await user.keyboard('jxk')
    expect(focusedSubject()).toBe('Second')

    await user.keyboard('e')
    expect(await screen.findByRole('status')).toBeTruthy()
    expect(subjects()).toEqual(['First'])

    await user.click(within(screen.getByRole('status')).getByRole('button', { name: 'Undo' }))
    await screen.findByText('Undid it')
    expect(subjects()).toEqual(['First', 'Second', 'Third'])
    // The click left the focus alone: keys still drive the list.
    expect(document.activeElement).toBe(document.body)
  })

  it('opens a message, reads it, and goes back to the list', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('j{Enter}')
    expect(await screen.findByText('Body of Second')).toBeTruthy()
    expect(location.pathname).toMatch(/^\/mail\/work\/INBOX\/\d+$/)
    const second = client.mail.list.value!.conversations.find((c) => c.latest.subject === 'Second')!.latest
    expect(server.labelsOf(second.id)).not.toContain(server.labelId('work', 'UNREAD'))
    // Reading quietly: no notice.
    expect(screen.queryByRole('status')).toBeNull()

    await user.keyboard('j')
    expect(await screen.findByText('Body of Third')).toBeTruthy()
    await user.keyboard('u')
    await screen.findByRole('list', { name: 'Messages' })
    expect(focusedSubject()).toBe('Third')
  })

  it('shows an HTML body in a sandbox, remote images only when asked, through the server', async () => {
    const first = server.deliver('work', 'Newsletter')
    // As the server rewrites it.
    const html = '<p>Hello <img src="/api/remote-image?url=https%3A%2F%2Ft.example%2Fpixel.gif"></p>'
    server.html.set(first.message, html)
    const user = await start('/mail/work/INBOX')
    await user.keyboard('{Enter}')
    const frame = (await screen.findByTitle('Message body')) as HTMLIFrameElement
    // Never scripts: with allow-same-origin, they could lift the sandbox.
    expect(frame.getAttribute('sandbox')).not.toContain('allow-scripts')
    expect(frame.srcdoc).toContain(html)
    // Nor lookups of the hosts linked to, which tell DNS servers it was read.
    expect(frame.srcdoc).toContain('<meta http-equiv="x-dns-prefetch-control" content="off">')
    expect(frame.srcdoc).toContain('img-src data:;')
    await user.click(screen.getByRole('button', { name: 'Show them' }))
    expect(frame.srcdoc).toContain(`img-src data: ${location.origin};`)
  })

  it("folds the quote of the mail answered, as Gmail does, but not a forward's", async () => {
    const reply = server.deliver('work', 'Re: Lunch')
    server.html.set(reply.message, '<p>Yes.</p><p>On Mon, Alice wrote:</p><blockquote>Lunch?</blockquote>')
    const user = await start('/mail/work/INBOX')
    await user.keyboard('{Enter}')
    const frame = (await screen.findByTitle('Message body')) as HTMLIFrameElement
    expect(frame.srcdoc).toContain('<div class="uplink-quote"><p>On Mon, Alice wrote:</p><blockquote>Lunch?</blockquote></div>')
    expect(frame.srcdoc).toContain('<html class="">')
    await user.click(screen.getByRole('button', { name: 'Show trimmed content' }))
    expect(frame.srcdoc).toContain('<html class="quote">')
    expect(screen.getByRole('button', { name: 'Hide trimmed content' }).getAttribute('aria-expanded')).toBe('true')
  })

  it('folds the quote of a plain-text message', async () => {
    const reply = server.deliver('work', 'Re: Lunch')
    server.text.set(reply.message, 'Yes.\n\nOn Mon, Alice wrote:\n> Lunch?')
    const user = await start('/mail/work/INBOX')
    await user.keyboard('{Enter}')
    await screen.findByText('Yes.')
    expect(screen.queryByText(/Lunch\?/)).toBeNull()
    await user.click(screen.getByRole('button', { name: 'Show trimmed content' }))
    expect(screen.getByText(/> Lunch\?/)).toBeTruthy()
  })

  it('shows a forward whole', async () => {
    const forward = server.deliver('work', 'Fwd: Lunch')
    server.text.set(forward.message, 'See below.\n\nOn Mon, Alice wrote:\n> Lunch?')
    const user = await start('/mail/work/INBOX')
    await user.keyboard('{Enter}')
    await screen.findByText(/> Lunch\?/)
    expect(screen.queryByRole('button', { name: 'Show trimmed content' })).toBeNull()
  })

  it('lists the files attached to a message, to download', async () => {
    const first = server.deliver('work', 'With files')
    server.attachments.set(first.message, [
      { index: 0, name: null, mime: 'image/png', size: 10, inline: true },
      { index: 1, name: 'report.pdf', mime: 'application/pdf', size: 123456, inline: false },
    ])
    const user = await start('/mail/work/INBOX')
    await user.keyboard('{Enter}')
    const list = await screen.findByRole('list', { name: 'Attachments' })
    const link = within(list).getByRole('link', { name: 'report.pdf' })
    expect(link.getAttribute('href')).toBe(`/api/messages/${first.message}/attachments/1`)
    expect(link.getAttribute('download')).toBe('report.pdf')
    expect(list.textContent).toBe('report.pdf 121 KB')
  })

  it('archives the open message and returns to the list', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('{Enter}')
    await screen.findByText('Body of First')
    await user.keyboard('e')
    await screen.findByRole('list', { name: 'Messages' })
    expect(subjects()).toEqual(['Second', 'Third'])
  })

  it('labels with l', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('l')
    const dialog = screen.getByRole('dialog', { name: 'Label' })
    expect(within(dialog).getAllByRole('option').map((o) => o.textContent)).toEqual([
      '☐Projects',
      '☐Travel',
    ])
    await user.keyboard('{Escape}')
    expect(screen.queryByRole('dialog')).toBeNull()

    // The filter has the focus as soon as the picker renders, before
    // effects run after paint, so typing straight on goes into it. (A
    // native event: testing-library's flush effects at once.)
    document.body.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'l', code: 'KeyL', bubbles: true }),
    )
    await new Promise((rendered) => setTimeout(rendered, 0))
    expect(document.activeElement).toBe(screen.getByRole('textbox', { name: 'Filter labels' }))
    await user.keyboard('trav{Enter}')
    expect(screen.queryByRole('dialog')).toBeNull()
    const first = client.mail.list.value!.conversations[0].latest
    expect(server.labelsOf(first.id)).toContain(server.labelId('work', 'Travel'))
    expect(within(rows()[0]).getByText('Travel')).toBeTruthy()
  })

  it('moves to a label with v, out of the label on show', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('v')
    const dialog = screen.getByRole('dialog', { name: 'Move to' })
    expect(within(dialog).getAllByRole('option').map((o) => o.textContent)).toEqual(['Projects', 'Travel'])
    const first = client.mail.list.value!.conversations[0].latest
    await user.keyboard('proj{Enter}')
    await waitFor(() => expect(subjects()).toEqual(['Second', 'Third']))
    expect(server.labelsOf(first.id)).toContain(server.labelId('work', 'Projects'))
    expect(server.labelsOf(first.id)).not.toContain(server.labelId('work', 'INBOX'))

    // From a user label, back to the Inbox.
    await user.keyboard('gl')
    await user.keyboard('proj{Enter}')
    await waitFor(() => expect(subjects()).toEqual(['First']))
    await user.keyboard('v')
    expect(
      within(screen.getByRole('dialog', { name: 'Move to' }))
        .getAllByRole('option')
        .map((o) => o.textContent),
    ).toEqual(['Inbox', 'Travel'])
    await user.keyboard('inbox{Enter}')
    await screen.findByText('No messages.')
    expect(server.labelsOf(first.id)).toContain(server.labelId('work', 'INBOX'))
    expect(server.labelsOf(first.id)).not.toContain(server.labelId('work', 'Projects'))
  })

  it('marks important with + or =, and not with -', async () => {
    const user = await start('/mail/work/INBOX')
    const important = server.labelId('work', 'IMPORTANT')
    const first = client.mail.list.value!.conversations[0].latest
    await user.keyboard('+')
    await waitFor(() => expect(server.labelsOf(first.id)).toContain(important))
    await user.keyboard('-')
    await waitFor(() => expect(server.labelsOf(first.id)).not.toContain(important))
    await user.keyboard('=')
    await waitFor(() => expect(server.labelsOf(first.id)).toContain(important))
  })

  it('selects read, unread, starred and unstarred messages with *', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('s{Shift>}i{/Shift}')
    await waitFor(() => expect(within(rows()[0]).getByTitle('Starred')).toBeTruthy())
    const selected = () => [...client.mail.selected.value].map((key) => client.mail.list.value!.conversations.find((c) => conversationKey(c) === key)!.latest.subject)
    await user.keyboard('*r')
    expect(selected()).toEqual(['First'])
    await user.keyboard('*u')
    expect(selected()).toEqual(['Second', 'Third'])
    await user.keyboard('*s')
    expect(selected()).toEqual(['First'])
    await user.keyboard('*t')
    expect(selected()).toEqual(['Second', 'Third'])
  })

  it('archives the open message with ] or [, opening the older or newer one', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('j{Enter}')
    await screen.findByText('Body of Second')
    await user.keyboard(']')
    await screen.findByText('Body of Third')
    expect(server.labelsOf(client.mail.list.value!.conversations[0].latest.id)).toContain(server.labelId('work', 'INBOX'))
    await user.keyboard('[[')
    await screen.findByText('Body of First')
    // Nothing newer: back to the list.
    await user.keyboard('[[')
    await screen.findByText('No messages.')
  })

  it('stars and unstars with a row\'s star, staying in the list', async () => {
    const user = await start('/mail/work/INBOX')
    const star = () => within(rows()[1]).getByRole('button', { name: 'Star' })
    expect(star().getAttribute('aria-pressed')).toBe('false')
    await user.click(star())
    await waitFor(() => expect(star().getAttribute('aria-pressed')).toBe('true'))
    expect(server.labelsOf(client.mail.list.value!.conversations[1].latest.id)).toContain(server.labelId('work', 'STARRED'))
    expect(location.pathname).toBe('/mail/work/INBOX')
    await user.click(star())
    await waitFor(() => expect(star().getAttribute('aria-pressed')).toBe('false'))
  })

  it('shows which messages have files attached', async () => {
    server.deliver('work', 'Report').has_attachments = true
    await start('/mail/work/INBOX')
    expect(within(rows()[0]).getByTitle('Has attachments')).toBeTruthy()
    expect(within(rows()[1]).queryByTitle('Has attachments')).toBeNull()
  })

  it('stars with s and marks unread with Shift+u', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('s')
    expect(within(rows()[0]).getByTitle('Starred')).toBeTruthy()
    await user.keyboard('s')
    expect(within(rows()[0]).queryByTitle('Starred')).toBeNull()

    await user.keyboard('{Shift>}i{/Shift}')
    expect(rows()[0].className).not.toMatch(/unread/)
    await user.keyboard('{Shift>}u{/Shift}')
    expect(rows()[0].className).toMatch(/unread/)
  })

  it('trashes with #, which hides the message from every other list', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('l')
    await user.keyboard('proj{Enter}')
    await user.keyboard('#')
    await waitFor(() => expect(subjects()).toEqual(['Second', 'Third']))
    await user.click(screen.getByRole('link', { name: /^Projects/ }))
    await screen.findByText('No messages.')
    await user.click(screen.getByRole('link', { name: /^Trash/ }))
    await waitFor(() => expect(subjects()).toEqual(['First']))

    await user.click(within(screen.getByRole('status')).getByRole('button', { name: 'Undo' }))
    await screen.findByText('No messages.')
    await user.keyboard('gi')
    await waitFor(() => expect(subjects()).toEqual(['First', 'Second', 'Third']))
  })

  it('lists All Mail with g a: archived mail too, but not trash', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('e')
    await waitFor(() => expect(subjects()).toEqual(['Second', 'Third']))
    await user.keyboard('#')
    await waitFor(() => expect(subjects()).toEqual(['Third']))
    await user.keyboard('ga')
    expect(location.pathname).toBe('/all/work')
    await waitFor(() => expect(subjects()).toEqual(['First', 'Third']))
    expect(screen.getByRole('link', { name: 'All Mail' }).getAttribute('aria-current')).toBe('page')
    await user.keyboard('{Enter}')
    expect(location.pathname).toMatch(/^\/all\/work\/\d+$/)
    await screen.findByText('Body of First')
  })

  it('reports spam with !, and in Spam, ! moves it back to the Inbox', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('!')
    await waitFor(() => expect(subjects()).toEqual(['Second', 'Third']))
    await user.click(screen.getByRole('link', { name: /^Spam/ }))
    await waitFor(() => expect(subjects()).toEqual(['First']))
    await user.keyboard('!')
    await screen.findByText('No messages.')
    await user.keyboard('gi')
    await waitFor(() => expect(subjects()).toEqual(['First', 'Second', 'Third']))
  })

  it('searches from the box that / focuses', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('/')
    expect(document.activeElement).toBe(screen.getByRole('searchbox'))
    await user.keyboard('second{Enter}')
    expect(location.pathname).toBe('/search/second')
    await screen.findByText('Second')
    expect(subjects()).toEqual(['Second'])
  })

  it('explains search, with examples to try', async () => {
    const user = await start('/mail/work/INBOX')
    await user.click(screen.getByRole('link', { name: 'Search help' }))
    expect(location.pathname).toBe('/help/search')
    await user.click(screen.getByRole('link', { name: 'in:inbox' }))
    expect(location.pathname).toBe('/search/in%3Ainbox')
    expect(screen.getByRole<HTMLInputElement>('searchbox').value).toBe('in:inbox')
  })

  it('jumps to labels with g', async () => {
    server.deliver('work', 'Outgoing', ['SENT'])
    const user = await start('/mail/work/INBOX')
    await user.keyboard('gt')
    expect(location.pathname).toBe('/mail/work/SENT')
    await screen.findByText('Outgoing')
  })

  it('takes g s as a sequence, without starring the message as s alone would', async () => {
    const user = await start('/mail/work/INBOX')
    const first = client.mail.list.value!.conversations[0].latest
    await user.keyboard('gs')
    expect(location.pathname).toBe('/mail/work/STARRED')
    await screen.findByText('No messages.')
    expect(server.labelsOf(first.id)).not.toContain(server.labelId('work', 'STARRED'))
    expect(server.requests.filter((r) => r.type === 'modify_labels')).toEqual([])
  })

  it('shows the shortcuts with ?', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('?')
    const help = screen.getByRole('dialog', { name: 'Keyboard shortcuts' })
    expect(within(help).getByText('Archive')).toBeTruthy()
    await user.keyboard('{Escape}')
    expect(screen.queryByRole('dialog')).toBeNull()
  })
})

describe('Microsoft 365', () => {
  let memo: Entry
  beforeEach(() => {
    server.addM365Account('office', ['Inbox/Projects'], ['Red'])
    memo = server.deliver('office', 'Memo')
  })

  it('lists the well-known folders in the nav, its drafts as Outlook’s', async () => {
    await start('/mail/office/INBOX')
    const office = within(screen.getByRole('navigation'))
      .getByText('office@example.com')
      .closest('section')!
    expect(
      within(office as HTMLElement)
        .getAllByRole('link')
        .map((l) => l.textContent),
    ).toEqual([
      'Inbox1',
      'Starred',
      'Important',
      'Sent',
      'Outlook drafts',
      'Archive',
      'Spam',
      'Trash',
      'All Mail',
      // Under an Inbox of its own, not a label: the folder's path.
      'Projects',
      'Red',
    ])
    expect(within(office as HTMLElement).getByRole('button', { name: 'Collapse Inbox' })).toBeTruthy()
    expect(within(screen.getByRole('navigation')).getByRole('link', { name: 'Gmail drafts' })).toBeTruthy()
  })

  it('opens every account’s Inbox as one, saying whose each message is', async () => {
    const user = await start('/inbox')
    expect(subjects()).toEqual(['Memo', 'First', 'Second', 'Third'])
    expect(rows().map((row) => row.querySelector('[class*=account]')!.textContent)).toEqual([
      'office@example.com',
      'work@example.com',
      'work@example.com',
      'work@example.com',
    ])
    const nav = screen.getByRole('navigation')
    const all = within(nav).getByRole('link', { name: /All inboxes/ })
    expect(all.textContent).toBe('All inboxes4')
    expect(all.getAttribute('aria-current')).toBe('page')

    // Archiving takes a message off the list; one moved back joins it.
    await user.keyboard('e')
    await waitFor(() => expect(subjects()).toEqual(['First', 'Second', 'Third']))
    server.relabel(memo.id, server.labelId('office', 'INBOX'), true)
    await waitFor(() => expect(subjects()).toEqual(['Memo', 'First', 'Second', 'Third']))

    // g i goes to the Inbox of the account on show, or else to all.
    await user.keyboard('gi')
    expect(location.pathname).toBe('/inbox')
    await user.keyboard('gs')
    expect(location.pathname).toBe('/mail/work/STARRED')
    await user.keyboard('gi')
    expect(location.pathname).toBe('/mail/work/INBOX')
  })

  it('moves to a folder with v, and labels with categories only', async () => {
    const user = await start('/mail/office/INBOX')
    await user.keyboard('l')
    expect(
      within(screen.getByRole('dialog', { name: 'Label' }))
        .getAllByRole('option')
        .map((o) => o.textContent),
    ).toEqual(['☐Red'])
    await user.keyboard('{Escape}')

    await user.keyboard('v')
    expect(
      within(screen.getByRole('dialog', { name: 'Move to' }))
        .getAllByRole('option')
        .map((o) => o.textContent),
    ).toEqual(['Sent', 'Outlook drafts', 'Archive', 'Spam', 'Trash', 'Inbox/Projects'])
    await user.keyboard('proj{Enter}')
    await screen.findByText('No messages.')
    expect(server.labelsOf(memo.id)).toContain(server.labelId('office', 'Inbox/Projects'))
    expect(server.labelsOf(memo.id)).not.toContain(server.labelId('office', 'INBOX'))
  })
})

describe('compose', () => {
  it('composes with c, saves as typed, and sends with Ctrl+Enter', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('c')
    const to = await screen.findByRole('textbox', { name: 'To' })
    expect(location.pathname).toMatch(/^\/compose\//)
    expect(document.activeElement).toBe(to)
    await user.keyboard('bob@example.org')
    await user.click(screen.getByRole('textbox', { name: 'Subject' }))
    await user.keyboard('Lunch')
    const [draft] = server.drafts.values()
    await waitFor(() => expect(draft.id && server.drafts.get(draft.id)!.content.subject).toBe('Lunch'))
    expect(server.drafts.get(draft.id)!.content.to).toBe('bob@example.org')
    // Opening it changed nothing else: as another tab opening it at once
    // would too, and the two would add up.
    expect(server.drafts.get(draft.id)!.content.body).toBe('')

    // Shortcuts work from within the fields, and nowhere else.
    await user.keyboard('{Control>}{Enter}{/Control}')
    await screen.findByText("Sent 'Lunch'")
    expect(server.outbox.map((i) => i.draft.content.subject)).toEqual(['Lunch'])
    expect(location.pathname).toBe('/mail/work/INBOX')

    // Undo puts it back in the drafts.
    await user.click(within(screen.getByRole('status')).getByRole('button', { name: 'Undo' }))
    await waitFor(() => expect(server.drafts.size).toBe(1))
    expect(server.outbox).toEqual([])
    await user.keyboard('gd')
    const drafts = await screen.findByRole('list', { name: 'Drafts' })
    expect(within(drafts).getByText('Lunch')).toBeTruthy()
  })

  it('replies to the open message with r', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('{Enter}')
    await screen.findByText('Body of First')
    await user.keyboard('r')
    const subject = await screen.findByRole('textbox', { name: 'Subject' })
    expect((subject as HTMLInputElement).value).toBe('Re: First')
    expect((screen.getByRole('textbox', { name: 'To' }) as HTMLInputElement).value).toBe('Bob <bob@example.org>')
    const [draft] = server.drafts.values()
    expect(draft.in_reply_to).toBe(client.mail.list.value!.conversations[0].latest.message)
    expect(draft.content.body).toContain('<blockquote><p>Body of First</p></blockquote>')
    await user.keyboard('{Escape}')
    await screen.findByText('Body of First')
  })

  it('attaches files, and removes them', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('c')
    await screen.findByRole('textbox', { name: 'To' })
    await user.upload(screen.getByLabelText('Files to attach'), [
      new File(['hello'], 'hello.txt', { type: 'text/plain' }),
      new File(['%PDF'], 'report.pdf', { type: 'application/pdf' }),
    ])
    const list = await screen.findByRole('list', { name: 'Attachments' })
    await waitFor(() => expect(within(list).getAllByRole('listitem')).toHaveLength(2))
    expect(list.textContent).toBe('hello.txt 5 B×report.pdf 4 B×')
    const [draft] = server.drafts.values()
    expect(server.drafts.get(draft.id)!.attachments.map((a) => [a.name, a.mime])).toEqual([
      ['hello.txt', 'text/plain'],
      ['report.pdf', 'application/pdf'],
    ])
    await user.click(within(list).getByRole('button', { name: 'Remove hello.txt' }))
    await waitFor(() => expect(within(list).getAllByRole('listitem')).toHaveLength(1))
    expect(server.drafts.get(draft.id)!.attachments.map((a) => a.name)).toEqual(['report.pdf'])
  })

  it('forwards the files of the open message with f', async () => {
    const first = server.deliver('work', 'With files')
    server.attachments.set(first.message, [
      { index: 0, name: null, mime: 'image/png', size: 10, inline: true },
      { index: 1, name: 'minutes.pdf', mime: 'application/pdf', size: 2048, inline: false },
    ])
    const user = await start('/mail/work/INBOX')
    await user.keyboard('{Enter}')
    await screen.findByRole('list', { name: 'Attachments' })
    await user.keyboard('f')
    const subject = await screen.findByRole('textbox', { name: 'Subject' })
    expect((subject as HTMLInputElement).value).toBe('Fwd: With files')
    const forwarded = await screen.findByRole('list', { name: 'Attachments' })
    expect(forwarded.textContent).toBe('minutes.pdf 2 KB×')
  })
})

describe('outbox', () => {
  it('lists what is not yet at the providers, by op, with what was refused', async () => {
    const user = await start('/outbox')
    await screen.findByText('The outbox is empty: everything you did is at the providers.')
    server.pend('work', { error: 'connection refused', since: Date.UTC(2026, 10, 10, 9) }, [
      { op: { id: 7, description: 'Starred 1 message', ts: Date.UTC(2026, 10, 10, 9) }, entries: 1, refused: [] },
      {
        op: { id: 5, description: 'Archived 3 messages', ts: Date.UTC(2026, 10, 10, 8) },
        entries: 3,
        refused: [{ reason: 'Invalid label', entries: 1 }],
      },
    ])
    const section = await screen.findByRole('region', { name: 'work' })
    expect(within(section).getByRole('alert').textContent).toMatch(/^Not reaching Gmail since .*: connection refused$/)
    const rows = within(section).getAllByRole('listitem')
    expect(rows.map((r) => r.querySelector('[class*=summary]')!.textContent)).toEqual([
      'Starred 1 message1 message waiting',
      'Archived 3 messages2 messages waiting; Gmail refused 1 message: Invalid label',
    ])
    // The nav counts both, and marks that something needs attention.
    const count = within(screen.getByRole('link', { name: /Outbox/ })).getByText('2')
    expect(count.title).toBe('Something needs your attention')

    // Keeping Gmail's resolves the refusal.
    await user.click(within(rows[1]).getByRole('button', { name: 'Keep Gmail’s' }))
    await screen.findByText('Kept what the provider has')
    await waitFor(() =>
      expect(within(section).queryByRole('button', { name: 'Keep Gmail’s' })).toBeNull(),
    )
    expect(within(section).getByText(/^2 messages waiting$/)).toBeTruthy()
  })

  it('lists a refused membership change until undone', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Party', false, 'invited')
    const user = await start(`/rooms/${room.id}`)
    await user.click(screen.getByRole('button', { name: 'Join' }))
    await screen.findByText('Joining…')
    server.refuseMembership(room.id, 'M_FORBIDDEN')
    expect((await screen.findByRole('alert')).textContent).toBe(
      'The homeserver refused: M_FORBIDDEN. Undo the change to keep the room as it is.',
    )
    expect(screen.getByText('not joined')).toBeTruthy()

    await user.click(screen.getByRole('link', { name: 'Undo the change' }))
    const section = await screen.findByRole('region', { name: 'mx' })
    const [row] = within(section).getAllByRole('listitem')
    expect(row.querySelector('[class*=summary]')!.textContent).toBe(
      "Joined 'Party'the homeserver refused: M_FORBIDDEN",
    )
    await user.click(within(row).getByRole('button', { name: 'Undo' }))
    await screen.findByText('The outbox is empty: everything you did is at the providers.')
  })

  it('lists Matrix messages sent though cancelled, until kept sent', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    const user = await start('/outbox')
    server.sentThoughCancelledToRoom(room.id, 'oops')
    const section = await screen.findByRole('region', { name: 'mx' })
    const [row] = within(section).getAllByRole('listitem')
    expect(row.querySelector('[class*=from]')!.textContent).toBe('Lounge')
    expect(row.querySelector('[class*=summary]')!.textContent).toBe(
      'oopsSent before the cancel could take it back',
    )
    expect(within(screen.getByRole('link', { name: /Outbox/ })).getByText('1').title).toBe(
      'Something needs your attention',
    )
    await user.click(within(row).getByRole('button', { name: 'Keep it sent' }))
    await screen.findByText('The outbox is empty: everything you did is at the providers.')
  })

  it('asks about mail sent though cancelled, until kept sent', async () => {
    const user = await start('/outbox')
    server.sentThoughCancelled('work', 'Lunch')
    const section = await screen.findByRole('region', { name: 'work' })
    expect(within(section).getByText('sent before the cancel could take it back')).toBeTruthy()
    const count = within(screen.getByRole('link', { name: /Outbox/ })).getByText('1')
    expect(count.title).toBe('Something needs your attention')
    await user.click(within(section).getByRole('button', { name: 'Keep it sent' }))
    await screen.findByText('The outbox is empty: everything you did is at the providers.')
  })
})

describe('matrix', () => {
  it('lists rooms in the nav, and reading one marks it read', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    server.receive(room.id, { body: 'Hello there' })
    const user = await start()
    const nav = screen.getByRole('navigation')
    // Its avatar, the initial, is not in its name.
    const link = await within(nav).findByRole('link', { name: 'Lounge1' })
    await user.click(link)
    expect(location.pathname).toBe(`/rooms/${room.id}`)
    expect(await screen.findByText('Hello there')).toBeTruthy()
    await waitFor(() => within(nav).getByRole('link', { name: 'Lounge' }))
    expect(link.getAttribute('aria-current')).toBe('page')
    // Reading quietly: no notice.
    expect(screen.queryByRole('status')).toBeNull()
  })

  it('counts messages that notify, and notifies of them while their room is not on show', async () => {
    const shown: Array<{ title: string; options: NotificationOptions; onclick: (() => void) | null }> = []
    class FakeNotification {
      static permission = 'granted'
      onclick: (() => void) | null = null
      constructor(
        readonly title: string,
        readonly options: NotificationOptions,
      ) {
        shown.push(this)
      }
      close() {}
    }
    vi.stubGlobal('Notification', FakeNotification)
    try {
      server.addMatrixAccount()
      const lounge = server.addRoom('Lounge')
      const quiet = server.addRoom('Quiet')
      server.quiet.add(quiet.id)
      // What was unread before is not news.
      server.receive(lounge.id, { body: 'Earlier' })
      await start('/todos')
      const nav = screen.getByRole('navigation')
      await within(nav).findByRole('link', { name: 'Lounge1' })

      server.receive(lounge.id, { body: 'Lunch?' })
      await waitFor(() => expect(shown).toHaveLength(1))
      expect(shown[0].title).toBe('Lounge')
      expect(shown[0].options).toEqual({ body: 'Bob: Lunch?', tag: `room-${lounge.id}` })

      // A quiet room notifies only of mentions, which its count shows.
      server.receive(quiet.id, { body: 'chatter' })
      server.receive(quiet.id, { body: `hey ${ME}` })
      await waitFor(() => expect(shown).toHaveLength(2))
      const count = within(within(nav).getByRole('link', { name: /^Quiet/ })).getByText('1')
      expect(count.title).toBe('You are mentioned')

      shown[1].onclick!()
      await within(await screen.findByRole('list', { name: 'Timeline' })).findByText(/^hey/)
      // Not of the room on show.
      server.receive(quiet.id, { body: `again ${ME}` })
      await within(screen.getByRole('list', { name: 'Timeline' })).findByText(/^again/)
      expect(shown).toHaveLength(2)
    } finally {
      vi.unstubAllGlobals()
    }
  })

  it('nests rooms in their spaces, which fold, counting what is unread', async () => {
    server.addMatrixAccount()
    const general = server.addRoom('General')
    const team = server.addRoom('Team')
    team.space = true
    team.children = [general.room_id]
    team.avatar = 'mxc://example.org/team'
    server.addRoom('Elsewhere')
    server.receive(general.id, { body: 'Hi' })
    const user = await start()
    const nav = screen.getByRole('navigation')
    const rooms = within(nav).getByRole('region', { name: `Rooms of ${ME}` })
    const space = (await within(rooms).findByText('Team')).closest('li')!
    expect(space.querySelector('img')?.getAttribute('src')).toBe(`/api/rooms/${team.id}/avatar/example.org/team`)
    expect(within(space).getByRole('link', { name: 'General1' })).toBeTruthy()
    expect(within(space).queryByRole('link', { name: 'Elsewhere' })).toBeNull()
    within(rooms).getByRole('link', { name: 'Elsewhere' })

    // The whole row folds it.
    const row = within(space).getByRole('button', { name: 'Team' })
    expect(row.getAttribute('aria-expanded')).toBe('true')
    await user.click(within(row).getByText('Team'))
    expect(within(space).queryByRole('link', { name: /General/ })).toBeNull()
    expect(within(space).getByRole('button', { name: 'Team1' }).getAttribute('aria-expanded')).toBe('false')
  })

  it('moves top-level spaces by dragging them, with undo', async () => {
    server.addMatrixAccount()
    for (const name of ['Alpha', 'Beta', 'Gamma']) server.addRoom(name).space = true
    // With a room in it, its row is a button, which folds it.
    server.rooms.find((r) => r.name === 'Gamma')!.children = [server.addRoom('Inner').room_id]
    const user = await start()
    const rooms = within(screen.getByRole('navigation')).getByRole('region', { name: `Rooms of ${ME}` })
    const order = () => [...rooms.querySelectorAll('[class*=parent] [class*=name]')].map((n) => n.textContent)
    const item = (name: string) => within(rooms).getByText(name).closest('li')!
    // None ordered yet: by room id.
    await waitFor(() => expect(order()).toEqual(['Alpha', 'Beta', 'Gamma']))

    // A press on the row lets the browser start a drag (preventing it
    // would not), and gives the focus back once it ends.
    const search = screen.getByRole('searchbox')
    search.focus()
    const gamma = within(rooms).getByRole('button', { name: 'Gamma' })
    expect(fireEvent.mouseDown(gamma)).toBe(true)
    gamma.focus()
    fireEvent.dragEnd(item('Gamma'))
    expect(document.activeElement).toBe(search)

    // Onto the top of Alpha's row: before it. Only Gamma needs an order.
    const dataTransfer = { setData: vi.fn(), effectAllowed: 'none' }
    fireEvent.dragStart(within(item('Gamma')).getByText('Gamma'), { dataTransfer })
    fireEvent.dragOver(item('Alpha'), { dataTransfer, clientY: 0 })
    expect(item('Alpha').className).toMatch(/dropBefore/)
    fireEvent.drop(item('Alpha'), { dataTransfer, clientY: 0 })
    await waitFor(() => expect(order()).toEqual(['Gamma', 'Alpha', 'Beta']))
    expect(item('Alpha').className).not.toMatch(/drop/)
    const notice = await screen.findByRole('status')
    expect(notice.textContent).toContain("Moved 'Gamma'")

    // Pending in the outbox until the homeserver has it.
    await user.click(within(screen.getByRole('navigation')).getByRole('link', { name: /Outbox/ }))
    const pending = await screen.findByText("Moved 'Gamma'", { selector: '[class*=subject]' })
    expect(pending.closest('li')!.textContent).toContain('to go: Gamma')
    server.spaceOrdersSet()
    await waitFor(() => expect(screen.queryByText("Moved 'Gamma'", { selector: '[class*=subject]' })).toBeNull())

    await user.click(within(screen.getByRole('status')).getByRole('button', { name: 'Undo' }))
    await waitFor(() => expect(order()).toEqual(['Alpha', 'Beta', 'Gamma']))
  })

  it('joins an invitation, and leaves the room, with undo in between', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Party', false, 'invited')
    const user = await start(`/rooms/${room.id}`)
    const nav = screen.getByRole('navigation')
    within(nav).getByRole('link', { name: 'Party (invited)' })
    expect(screen.queryByRole('textbox', { name: /message/i })).toBeNull()

    await user.click(screen.getByRole('button', { name: 'Join' }))
    await screen.findByText('Joining…')
    // Changed one's mind before it took effect.
    await user.click(within(screen.getByRole('status')).getByRole('button', { name: 'Undo' }))
    await screen.findByRole('button', { name: 'Join' })

    await user.click(screen.getByRole('button', { name: 'Join' }))
    server.settleMembership(room.id)
    await screen.findByRole('button', { name: 'Leave' })
    within(nav).getByRole('link', { name: 'Party' })

    await user.click(screen.getByRole('button', { name: 'Leave' }))
    await screen.findByText('Leaving…')
    server.settleMembership(room.id)
    await screen.findByText('No such room.')
    expect(within(nav).queryByRole('link', { name: /^Party/ })).toBeNull()
  })

  it('tags a room by l, a favourite not of low priority, and new tags of the user', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    server.addRoom('Work').tags = ['u.work']
    const user = await start(`/rooms/${room.id}`)
    const tags = () => [...screen.getByRole('button', { name: 'Tags' }).querySelectorAll('[class*=chip]')].map((c) => c.textContent)
    expect(tags()).toEqual([])

    await user.keyboard('l')
    const picker = screen.getByRole('dialog', { name: 'Tag' })
    expect(within(picker).getAllByRole('option').map((o) => o.textContent)).toEqual(['☐Favourite', '☐Low priority', '☐work'])
    await user.keyboard('low{Enter}')
    await waitFor(() => expect(tags()).toEqual(['Low priority']))
    expect(server.requests).toContainEqual({ type: 'modify_room_tags', rooms: [room.id], add: ['m.lowpriority'], remove: [] })

    // A favourite is no longer of low priority.
    await user.click(screen.getByRole('button', { name: 'Tags' }))
    await user.click(within(screen.getByRole('dialog', { name: 'Tag' })).getByRole('option', { name: '☐Favourite' }))
    await waitFor(() => expect(tags()).toEqual(['Favourite']))

    await user.keyboard('l')
    await user.keyboard('Reading list')
    const options = within(screen.getByRole('dialog', { name: 'Tag' })).getAllByRole('option')
    expect(options.map((o) => o.textContent)).toEqual(['☐New tag: Reading list'])
    await user.keyboard('{Enter}')
    await waitFor(() => expect(tags()).toEqual(['Favourite', 'Reading list']))

    // Untagged by choosing it again; undone, it is back.
    await user.keyboard('l')
    await user.click(within(screen.getByRole('dialog', { name: 'Tag' })).getByRole('option', { name: '☑Favourite' }))
    await waitFor(() => expect(tags()).toEqual(['Reading list']))
    await user.click(within(screen.getByRole('status')).getByRole('button', { name: 'Undo' }))
    await waitFor(() => expect(tags()).toEqual(['Favourite', 'Reading list']))
  })

  it('lists tags being set in the outbox, until the homeserver has them', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    const user = await start(`/rooms/${room.id}`)
    await user.keyboard('l')
    await user.keyboard('fav{Enter}')
    await waitFor(() => expect(room.tags).toEqual(['m.favourite']))
    await user.click(within(screen.getByRole('navigation')).getByRole('link', { name: /^Outbox/ }))
    const pending = await screen.findByText("Tagged 'Lounge' m.favourite", { selector: '[class*=subject]' })
    expect(pending.closest('li')!.textContent).toContain('to go: +Favourite Lounge')
    server.roomTagsSet()
    await screen.findByText(/^The outbox is empty/)
  })

  it('shows events it does not display otherwise as raw JSON', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge', true)
    server.receive(room.id, {
      kind: 'org.example.custom',
      msgtype: null,
      raw: JSON.stringify({ type: 'org.example.custom', content: { answer: 42 } }),
    })
    server.receive(room.id, { kind: 'm.room.encrypted', msgtype: null, encryption: 'undecryptable' })
    await start(`/rooms/${room.id}`)
    expect(await screen.findByText('org.example.custom')).toBeTruthy()
    expect(screen.getByText(/"answer": 42/)).toBeTruthy()
    expect(screen.getByText('Unable to decrypt this message.')).toBeTruthy()
  })

  it('sends with Enter, showing the message until it is synced back', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    const user = await start(`/rooms/${room.id}`)
    await user.keyboard('r')
    const box = screen.getByRole('textbox', { name: 'Message Lounge' })
    expect(document.activeElement).toBe(box)
    await user.keyboard('First line{Shift>}{Enter}{/Shift}second line{Enter}')
    expect(await screen.findByText('Waiting to send')).toBeTruthy()
    expect(screen.getByText('First line second line')).toBeTruthy()
    expect((box as HTMLTextAreaElement).value).toBe('')
    expect(server.requests).toContainEqual({ type: 'send_matrix_message', room: room.id, content: text('First line\nsecond line') })

    server.echo(server.matrixSends[0].id)
    await waitFor(() => expect(screen.queryByText('Waiting to send')).toBeNull())
    expect(screen.getByText('First line second line')).toBeTruthy()
  })
})

describe('room composer', () => {
  it('completes members, emoji and commands, and runs commands', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    const member = (user: string, name: string) => ({
      user,
      name,
      membership: 'joined' as const,
      power: 0,
      kickable: false,
      bannable: false,
    })
    server.members.set(room.id, {
      members: [member(ME, 'Me'), member('@carol:example.org', 'Carol Danvers'), member('@bob:example.org', 'Bob')],
      powers: { invite: true, name: true, topic: true },
    })
    const user = await start(`/rooms/${room.id}`)
    const box = screen.getByRole('textbox', { name: 'Message Lounge' }) as HTMLTextAreaElement
    const options = () => within(screen.getByRole('listbox', { name: 'Suggestions' })).getAllByRole('option')
    const sent = () => server.requests.filter((r) => r.type === 'send_matrix_message').map((r) => r.content)

    await user.click(box)
    await user.keyboard('hi @dan')
    await waitFor(() => expect(options().map((o) => o.textContent)).toEqual(['Carol Danvers@carol:example.org']))
    await user.keyboard('{Tab}')
    expect(box.value).toBe('hi [Carol Danvers](https://matrix.to/#/@carol:example.org) ')
    expect(screen.queryByRole('listbox')).toBeNull()
    await user.keyboard(':thumbsu')
    await waitFor(() => expect(options()[0].textContent).toContain('👍'))
    await user.keyboard('{Enter}')
    // A whole shortcode becomes its emoji.
    await user.keyboard(' :tada:')
    expect(box.value).toBe('hi [Carol Danvers](https://matrix.to/#/@carol:example.org) 👍 🎉')
    await user.keyboard('{Enter}')
    await waitFor(() =>
      expect(sent()).toEqual([text('hi [Carol Danvers](https://matrix.to/#/@carol:example.org) 👍 🎉')]),
    )

    // Escape dismisses a completion; Enter then sends.
    await user.keyboard('@b')
    await waitFor(() => expect(options()).toHaveLength(1))
    await user.keyboard('{Escape}')
    expect(screen.queryByRole('listbox')).toBeNull()
    await user.keyboard('{Backspace}{Backspace}')

    await user.keyboard('/inv')
    expect(options().map((o) => o.textContent)).toEqual(['/invite <user-id>Invites the user to the room'])
    await user.keyboard('{Tab}@dave:example.org{Enter}')
    await user.keyboard('/plain **as is**{Enter}')
    await waitFor(() =>
      expect(sent().slice(1)).toEqual([
        { type: 'member', action: 'invite', user: '@dave:example.org', reason: null },
        text('**as is**', { markdown: false }),
      ]),
    )
    await user.keyboard('/frobnicate{Enter}')
    expect(screen.getByRole('alert').textContent).toBe(
      'Unknown command /frobnicate. To send it as a message, begin with //.',
    )
    expect(box.value).toBe('/frobnicate')
  })
})

describe('room timeline', () => {
  const day = (iso: string) => new Date(iso).getTime()
  const timeline = () => screen.getByRole('list', { name: 'Timeline' })
  const line = (text: string | RegExp) => within(timeline()).getByText(text).closest('li')!
  const member = (user: string, membership: string, prev?: string) =>
    JSON.stringify({ type: 'm.room.member', state_key: user, content: { membership }, unsigned: prev ? { prev_content: { membership: prev } } : {} })

  it('lays events out a line each, with days apart and a line where the unread begin', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    const read = server.receive(room.id, { body: 'Yesterday', ts: day('2026-11-09T12:00') })
    server.receive(room.id, { body: 'Mine', sender: ME, sender_name: 'Me', ts: day('2026-11-10T09:00') })
    server.receive(room.id, { body: 'Unread', ts: day('2026-11-10T09:05') })
    room.read_up_to = server.marker(room.id, read.id)
    await start(`/rooms/${room.id}`)
    await within(timeline()).findByText('Unread')
    const separators = within(timeline()).getAllByRole('separator')
    expect(separators.map((s) => s.textContent)).toEqual(['2026-11-09', '2026-11-10', 'New'])
    // After the account's own: the unread are those of others.
    expect(line('Unread').previousElementSibling).toBe(separators[2])
    const unread = line('Unread')
    expect(unread.querySelector('time')!.textContent).toBe('09:05')
    expect(within(unread).getByTitle('@bob:example.org').textContent).toContain('Bob')
  })

  it('summarizes state events, folding runs of membership changes', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    server.receive(room.id, {
      kind: 'm.room.topic',
      msgtype: null,
      raw: JSON.stringify({ type: 'm.room.topic', state_key: '', content: { topic: 'Cats' } }),
    })
    server.receive(room.id, { kind: 'm.room.member', msgtype: null, raw: member('@bob:example.org', 'join') })
    server.receive(room.id, { body: 'Hi' })
    for (const name of ['carol', 'dave', 'erin']) {
      server.receive(room.id, {
        kind: 'm.room.member',
        msgtype: null,
        sender: `@${name}:example.org`,
        sender_name: null,
        raw: member(`@${name}:example.org`, 'leave', 'join'),
      })
    }
    const user = await start(`/rooms/${room.id}`)
    await within(timeline()).findByText('Bob changed the topic to "Cats"')
    within(timeline()).getByText('Bob joined')
    const run = within(timeline()).getByText('3 membership changes: carol left, dave left…')
    await user.click(run)
    within(timeline()).getByText('erin left')
  })

  it('shows HTML, media, and edits with their history', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    server.receive(room.id, { body: '**bold**', html: '<strong>bold</strong> <a href="https://x.org">x</a>' })
    const image = server.receive(room.id, {
      msgtype: 'm.image',
      body: 'cat.png',
      raw: JSON.stringify({
        content: { msgtype: 'm.image', body: 'cat.png', url: 'mxc://example.org/cat', info: { w: 640, h: 480 } },
      }),
    })
    server.receive(room.id, {
      msgtype: 'm.file',
      body: 'notes.pdf',
      raw: JSON.stringify({ content: { msgtype: 'm.file', body: 'notes.pdf', url: 'mxc://example.org/n', info: { size: 2048 } } }),
    })
    const edited = server.receive(room.id, { body: 'Frist' })
    edited.edit = { event_id: '$e2', ts: edited.ts + 2, content: JSON.stringify({ msgtype: 'm.text', body: 'First' }), html: null }
    const edit = (body: string, ts: number) =>
      ({ ...edited, edit: null, raw: JSON.stringify({ content: { 'm.new_content': { body } } }), ts })
    server.edits.set(edited.id, [edit('Firs', edited.ts + 1), edit('First', edited.ts + 2)])
    const user = await start(`/rooms/${room.id}`)

    const bold = await within(timeline()).findByText('bold')
    expect(bold.tagName).toBe('STRONG')
    within(timeline()).getByRole('link', { name: 'x' })
    const img = within(timeline()).getByRole('img', { name: 'cat.png' })
    expect(img.getAttribute('src')).toBe(`/api/rooms/${room.id}/events/${image.id}/thumbnail/example.org/cat`)
    expect([img.getAttribute('width'), img.getAttribute('height')]).toEqual(['320', '240'])
    const file = within(timeline()).getByRole('link', { name: /notes\.pdf/ })
    expect(file.textContent).toBe('notes.pdf (2 KB)')

    expect(within(line('First')).queryByText('Frist')).toBeNull()
    await user.click(within(line('First')).getByRole('button', { name: '(edited)' }))
    const history = await screen.findByRole('dialog', { name: 'Edit history' })
    await within(history).findByText('Firs')
    expect([...history.querySelectorAll('li div')].map((d) => d.textContent)).toEqual(['Frist', 'Firs', 'First'])
    await user.keyboard('{Escape}')
    expect(screen.queryByRole('dialog')).toBeNull()
  })

  it('reacts, showing the reaction being sent, and takes it back', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    const event = server.receive(room.id, {
      body: 'Lunch?',
      reactions: [{ key: '👍', senders: [{ sender: '@bob:example.org', sender_name: 'Bob', event_id: '$r1' }] }],
    })
    const user = await start(`/rooms/${room.id}`)
    await within(timeline()).findByText('Lunch?')
    const chip = within(line('Lunch?')).getByRole('button', { name: '👍 1' })
    expect(chip.getAttribute('title')).toBe('Bob reacted with 👍')
    await user.click(chip)
    expect(server.requests).toContainEqual({
      type: 'send_matrix_message',
      room: room.id,
      content: { type: 'reaction', event: event.event_id, key: '👍' },
    })
    await within(line('Lunch?')).findByRole('button', { name: '👍 2', pressed: true })
    // Not listed apart while it goes out.
    expect(within(timeline()).queryByText('Waiting to send')).toBeNull()

    server.change(room.id, event, {
      reactions: [
        {
          key: '👍',
          senders: [
            { sender: '@bob:example.org', sender_name: 'Bob', event_id: '$r1' },
            { sender: ME, sender_name: 'Me', event_id: '$r2' },
          ],
        },
      ],
    })
    server.settleSends(room.id)
    await user.click(await within(line('Lunch?')).findByRole('button', { name: '👍 2', pressed: true }))
    expect(server.requests).toContainEqual({ type: 'send_matrix_message', room: room.id, content: { type: 'redaction', event: '$r2' } })
    await within(line('Lunch?')).findByRole('button', { name: '👍 1', pressed: false })

    // Any other, from the picker.
    await user.click(within(line('Lunch?')).getByRole('button', { name: 'React' }))
    await user.click(screen.getByRole('button', { name: 'React with 🎉' }))
    expect(server.requests).toContainEqual({
      type: 'send_matrix_message',
      room: room.id,
      content: { type: 'reaction', event: event.event_id, key: '🎉' },
    })
  })

  it('replies, edits the last message with Up, and deletes', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    const question = server.receive(room.id, { body: 'Lunch?' })
    const mine = server.receive(room.id, { body: 'Sure', sender: ME, sender_name: 'Me' })
    const user = await start(`/rooms/${room.id}`)
    await within(timeline()).findByText('Sure')
    await user.click(within(line('Lunch?')).getByRole('button', { name: 'Reply' }))
    const box = screen.getByRole('textbox', { name: 'Message Lounge' })
    expect(document.activeElement).toBe(box)
    screen.getByText(/^Replying to/)
    await user.keyboard('Where?{Enter}')
    expect(server.requests).toContainEqual({
      type: 'send_matrix_message',
      room: room.id,
      content: text('Where?', { reply_to: question.event_id }),
    })
    await waitFor(() => expect(screen.queryByText(/^Replying to/)).toBeNull())

    await user.keyboard('{ArrowUp}')
    expect((box as HTMLTextAreaElement).value).toBe('Sure')
    await user.keyboard(' thing{Enter}')
    expect(server.requests).toContainEqual({
      type: 'send_matrix_message',
      room: room.id,
      content: text('Sure thing', { replaces: mine.event_id }),
    })
    // Shown on the message while it goes out.
    await within(timeline()).findByText('(saving…)')
    expect((box as HTMLTextAreaElement).value).toBe('')

    // An edit cancelled leaves the draft as it was.
    await user.keyboard('draft')
    await user.click(within(line('Lunch?')).getByRole('button', { name: 'Reply' }))
    await user.keyboard('{Escape}')
    expect((box as HTMLTextAreaElement).value).toBe('draft')

    await user.click(within(line(/^Sure/)).getByRole('button', { name: 'Options' }))
    await user.click(screen.getByRole('menuitem', { name: 'Delete' }))
    expect(server.requests).toContainEqual({ type: 'send_matrix_message', room: room.id, content: { type: 'redaction', event: mine.event_id } })
    await within(timeline()).findByText('Deleting…')
  })

  it('previews the pages messages link, but in encrypted rooms', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    const secret = server.addRoom('Secret', true)
    const url = 'https://example.org/post'
    server.previews.set(url, {
      title: 'A post',
      description: 'About things',
      site_name: 'Example',
      image: 'mxc://example.org/img',
      image_width: 1200,
      image_height: 630,
    })
    const message = server.receive(room.id, { body: `Read ${url}, and https://matrix.to/#/@bob:example.org` })
    server.receive(secret.id, { body: `Read ${url}` })
    const user = await start(`/rooms/${room.id}`)
    const preview = await within(timeline()).findByRole('link', { name: /A post/ })
    expect(preview.getAttribute('href')).toBe(url)
    expect(preview.textContent).toContain('About things')
    expect(preview.querySelector('img')!.getAttribute('src')).toBe(
      `/api/rooms/${room.id}/events/${message.id}/preview/example.org/img?url=${encodeURIComponent(url)}`,
    )
    // Only the page's: not the user's.
    expect(server.requests.filter((r) => r.type === 'preview_url')).toEqual([
      { type: 'preview_url', room: room.id, event: message.id, url },
    ])

    await user.click(await screen.findByRole('link', { name: /^Secret/ }))
    await screen.findByRole('heading', { name: 'Secret' })
    await within(timeline()).findByRole('link', { name: url })
    expect(server.requests.filter((r) => r.type === 'preview_url')).toHaveLength(1)
  })

  it('searches the room, showing what it finds in the timeline or its thread', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    const old = server.receive(room.id, { body: 'Lunch at noon?' })
    for (let i = 0; i < TIMELINE_PAGE; i++) server.receive(room.id, { body: `filler ${i}` })
    const root = server.receive(room.id, { body: 'Plans' })
    server.receiveInThread(room.id, root, { body: 'lunch then' })
    const user = await start(`/rooms/${room.id}`)
    await within(timeline()).findByText('Plans')
    expect(within(timeline()).queryByText('Lunch at noon?')).toBeNull()

    // The box at the top searches the room on show, not mail.
    await user.keyboard('/')
    expect(document.activeElement).toBe(screen.getByRole('searchbox', { name: 'Search Lounge' }))
    expect(screen.queryByRole('link', { name: 'Search help' })).toBeNull()
    await user.keyboard('LUNCH{Enter}')
    expect(location.pathname).toBe(`/rooms/${room.id}/search/LUNCH`)
    const search = screen.getByRole('complementary', { name: 'Search' })
    const results = await within(search).findByRole('list', { name: 'Results' })
    const found = within(results).getAllByRole('button')
    expect(found.map((b) => b.textContent)).toEqual([
      expect.stringContaining('lunch then'),
      expect.stringContaining('Lunch at noon?'),
    ])
    expect(within(found[1]).getByText('Lunch').tagName).toBe('MARK')

    // One far back: earlier pages load until it shows.
    await user.click(found[1])
    await within(timeline()).findByText('Lunch at noon?')
    expect(line('Lunch at noon?').id).toBe(`event-${old.id}`)
    // One of a thread: its thread opens.
    await user.click(found[0])
    await within(screen.getByRole('complementary', { name: 'Thread' })).findByText('lunch then')
    expect(location.pathname).toBe(`/rooms/${room.id}`)
    expect(screen.getByRole<HTMLInputElement>('searchbox').value).toBe('')

    // Searching again shows over the thread; closing it shows the thread.
    await user.type(screen.getByRole('searchbox'), 'plans{Enter}')
    await within(screen.getByRole('complementary', { name: 'Search' })).findByRole('list', { name: 'Results' })
    expect(screen.queryByRole('complementary', { name: 'Thread' })).toBeNull()
    await user.click(screen.getByRole('button', { name: 'Close search' }))
    expect(location.pathname).toBe(`/rooms/${room.id}`)
    screen.getByRole('complementary', { name: 'Thread' })
  })

  it('pins messages, which a bar under the header shows in turn', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    const old = server.receive(room.id, { body: 'Rules: be kind' })
    for (let i = 0; i < TIMELINE_PAGE; i++) server.receive(room.id, { body: `filler ${i}` })
    const latest = server.receive(room.id, { body: 'Meet at noon' })
    const user = await start(`/rooms/${room.id}`)
    await within(timeline()).findByText('Meet at noon')
    expect(screen.queryByRole('button', { name: 'Pinned messages' })).toBeNull()

    await user.click(within(line('Meet at noon')).getByRole('button', { name: 'Options' }))
    await user.click(screen.getByRole('menuitem', { name: 'Pin' }))
    expect(server.requests).toContainEqual({
      type: 'send_matrix_message',
      room: room.id,
      content: { type: 'pin', event: latest.event_id, pinned: true },
    })
    // As the sync brings it back, with one pinned before.
    server.pin(room.id, [old.event_id, latest.event_id])
    const bar = await screen.findByRole('button', { name: 'Pinned messages' })
    await within(bar).findByText(/Meet at noon/)
    expect(bar.textContent).toContain('1 of 2')

    // A click shows it, and the next: one far back loads until it shows.
    await user.click(bar)
    await within(bar).findByText(/Rules: be kind/)
    await user.click(bar)
    await within(timeline()).findByText('Rules: be kind')
    expect(line('Rules: be kind').id).toBe(`event-${old.id}`)

    await user.click(within(line('Rules: be kind')).getByRole('button', { name: 'Options' }))
    await user.click(screen.getByRole('menuitem', { name: 'Unpin' }))
    expect(server.requests).toContainEqual({
      type: 'send_matrix_message',
      room: room.id,
      content: { type: 'pin', event: old.event_id, pinned: false },
    })
  })

  it('opens threads beside the timeline, which sum them up, and replies in them', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    const root = server.receive(room.id, { body: 'Plans?' })
    server.receive(room.id, { body: 'Main line' })
    server.receiveInThread(room.id, root, { body: 'Saturday', sender: '@carol:example.org', sender_name: 'Carol' })
    const user = await start(`/rooms/${room.id}`)
    await within(timeline()).findByText('Main line')
    // The thread's message is the thread's; its root sums it up.
    expect(within(timeline()).getAllByText('Saturday')).toHaveLength(1)
    const summary = within(line('Plans?')).getByRole('button', { name: /1 reply/ })
    expect(summary.textContent).toContain('Saturday')

    await user.click(summary)
    const thread = screen.getByRole('complementary', { name: 'Thread' })
    await within(thread).findByText('Saturday')
    within(thread).getByText('Plans?')
    const box = within(thread).getByRole('textbox', { name: 'Reply in thread' })
    expect(within(thread).queryByRole('button', { name: 'Attach files' })).toBeNull()
    await user.type(box, 'Works for me{Enter}')
    expect(server.requests).toContainEqual({
      type: 'send_matrix_message',
      room: room.id,
      content: text('Works for me', { thread: root.event_id }),
    })
    // Being sent, in the thread, not the timeline.
    await within(thread).findByText('Works for me')
    expect(within(timeline()).queryByText('Works for me')).toBeNull()

    // A reply within the thread.
    const threadLine = within(thread).getByText('Saturday').closest('li')!
    await user.click(within(threadLine).getByRole('button', { name: 'Reply' }))
    await user.keyboard('Which time?{Enter}')
    expect(server.requests).toContainEqual({
      type: 'send_matrix_message',
      room: room.id,
      content: text('Which time?', { thread: root.event_id, reply_to: '$3' }),
    })

    // As more come, the thread and its summary follow.
    server.receiveInThread(room.id, root, { body: 'Noon' })
    await within(thread).findByText('Noon')
    await within(line('Plans?')).findByRole('button', { name: /2 replies/ })

    await user.click(within(thread).getByRole('button', { name: 'Close thread' }))
    expect(screen.queryByRole('complementary', { name: 'Thread' })).toBeNull()
    // Any other message may start one.
    await user.click(within(line('Main line')).getByRole('button', { name: 'Reply in thread' }))
    await within(screen.getByRole('complementary', { name: 'Thread' })).findByText('Main line')
  })

  it('shows who is typing and how far members have read, and says when the account types', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    const first = server.receive(room.id, { body: 'First' })
    const second = server.receive(room.id, { body: 'Second' })
    const user = await start(`/rooms/${room.id}`)
    await within(timeline()).findByText('Second')

    server.typing(room.id, [{ user: '@bob:example.org', name: 'Bob' }])
    await screen.findByText('Bob is typing…')
    server.typing(room.id, [
      { user: '@bob:example.org', name: 'Bob' },
      { user: '@carol:example.org', name: null },
    ])
    await screen.findByText('Bob and carol are typing…')
    server.typing(room.id, [])
    await waitFor(() => expect(screen.queryByText(/typing/)).toBeNull())

    // On the last listed event at or before the one read.
    server.read(room.id, [
      { member: { user: '@bob:example.org', name: 'Bob' }, event: first.id, pos: first.pos, ts: null },
      // An event not listed, as a reaction.
      { member: { user: '@carol:example.org', name: 'Carol' }, event: second.id + 10, pos: second.pos + 10, ts: null },
    ])
    await within(line('First')).findByLabelText('Read by Bob')
    within(line('Second')).getByLabelText('Read by Carol')

    await user.keyboard('r')
    await user.keyboard('h')
    expect(server.requests).toContainEqual({ type: 'set_typing', room: room.id, typing: true })
    await user.keyboard('{Backspace}')
    expect(server.requests).toContainEqual({ type: 'set_typing', room: room.id, typing: false })
  })

  it('shows events decrypted late, and drops those that turn out reactions', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge', true)
    const hello = server.receive(room.id, { body: 'Hello' })
    const secret = server.receive(room.id, { kind: 'm.room.encrypted', msgtype: null, encryption: 'undecryptable' })
    const reaction = server.receive(room.id, { kind: 'm.room.encrypted', msgtype: null, encryption: 'undecryptable' })
    await start(`/rooms/${room.id}`)
    await waitFor(() => expect(within(timeline()).getAllByText(/Unable to decrypt/)).toHaveLength(2))

    server.change(room.id, secret, { kind: 'm.room.message', msgtype: 'm.text', body: 'Secret', encryption: 'decrypted' })
    await within(timeline()).findByText('Secret')
    server.relation(room.id, reaction, hello, {
      reactions: [{ key: '👍', senders: [{ sender: '@bob:example.org', sender_name: 'Bob', event_id: reaction.event_id }] }],
    })
    await waitFor(() => expect(within(timeline()).queryByText(/Unable to decrypt/)).toBeNull())
    within(line('Hello')).getByRole('button', { name: /👍/ })
  })

  it('creates rooms, starts direct chats and joins rooms, opening them', async () => {
    server.addMatrixAccount()
    server.addRoom('Lobby', false, 'left')
    const user = await start()
    const starter = async () => {
      await user.click(await screen.findByRole('button', { name: 'Start a conversation' }))
      return screen.getByRole('dialog', { name: 'Start a conversation' })
    }
    let dialog = await starter()
    await user.type(within(dialog).getByLabelText('Name'), 'Plans')
    await user.type(within(dialog).getByLabelText('Invite'), '@bob:example.org, @carol:example.org')
    await user.click(within(dialog).getByRole('button', { name: 'Create room' }))
    expect(await screen.findByRole('heading', { name: 'Plans' })).toBeTruthy()
    expect(screen.queryByRole('dialog')).toBeNull()
    expect(server.requests.find((r) => r.type === 'create_matrix_room')).toEqual({
      type: 'create_matrix_room',
      account: 'mx',
      room: { name: 'Plans', topic: null, invite: ['@bob:example.org', '@carol:example.org'], encrypted: true, alias: null },
    })

    dialog = await starter()
    await user.click(within(dialog).getByRole('tab', { name: 'Direct chat' }))
    await user.type(within(dialog).getByLabelText('User'), 'bob{Enter}')
    expect(await within(dialog).findByRole('alert')).toHaveProperty('textContent', 'not a Matrix user id: "bob"')
    await user.type(within(dialog).getByLabelText('User'), ':example.org')
    await user.clear(within(dialog).getByLabelText('User'))
    await user.type(within(dialog).getByLabelText('User'), '@bob:example.org{Enter}')
    expect(await screen.findByRole('heading', { name: 'bob' })).toBeTruthy()

    dialog = await starter()
    await user.click(within(dialog).getByRole('tab', { name: 'Join a room' }))
    await user.type(within(dialog).getByLabelText('Address or id'), '#lobby:example.org{Enter}')
    expect(await screen.findByRole('heading', { name: 'Lobby' })).toBeTruthy()
  })

  it('lists members by power, and changes them, and the name, from the room info', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    room.joined_members = 3
    const member = (user: string, name: string | null, power: number | null, membership: MemberState = 'joined') => ({
      user,
      name,
      power,
      membership,
      kickable: power !== null && power < 50,
      bannable: power !== null && power < 50,
    })
    server.members.set(room.id, {
      members: [
        member('@carol:example.org', 'Carol', 0),
        member(ME, 'Me', 50),
        member('@bob:example.org', 'Bob', null),
        member('@dave:example.org', null, 0, 'invited'),
        member('@eve:example.org', 'Eve', 0, 'banned'),
        member('@gone:example.org', 'Gone', 0, 'left'),
      ],
      powers: { invite: true, name: true, topic: false },
    })
    const user = await start(`/rooms/${room.id}`)
    await user.click(await screen.findByRole('button', { name: '3 members' }))
    const info = await screen.findByRole('complementary', { name: 'Room info' })
    const joined = await within(info).findByRole('region', { name: 'Joined' })
    expect(within(joined).getAllByRole('listitem').map((li) => li.getAttribute('aria-label'))).toEqual([
      'Bob',
      'Me',
      'Carol',
    ])
    expect(within(within(joined).getByRole('listitem', { name: 'Bob' })).getByText('Owner')).toBeTruthy()
    expect(within(within(joined).getByRole('listitem', { name: 'Bob' })).queryByRole('button')).toBeNull()
    expect(within(info).queryByText('Gone')).toBeNull()

    // Removing, with a reason.
    await user.click(within(within(joined).getByRole('listitem', { name: 'Carol' })).getByRole('button', { name: 'Remove' }))
    await user.type(within(info).getByLabelText('Reason'), 'noisy{Enter}')
    const invited = within(info).getByRole('region', { name: 'Invited' })
    await user.click(within(invited).getByRole('button', { name: 'Revoke invitation' }))
    await user.click(within(invited).getByRole('button', { name: 'Revoke invitation' }))
    const banned = within(info).getByRole('region', { name: 'Banned' })
    await user.click(within(banned).getByRole('button', { name: 'Unban' }))
    await user.click(within(banned).getByRole('button', { name: 'Unban' }))
    await user.type(within(info).getByLabelText('User to invite'), '@frank:example.org{Enter}')
    // The topic is not the account's to change; the name is.
    expect(within(info).getByLabelText('Topic')).toHaveProperty('disabled', true)
    const name = within(info).getByLabelText('Name')
    await user.clear(name)
    await user.type(name, 'Parlour')
    await user.click(within(info).getByRole('button', { name: 'Save' }))
    await waitFor(() =>
      expect(server.requests.filter((r) => r.type === 'send_matrix_message').map((r) => r.content)).toEqual([
        { type: 'member', user: '@carol:example.org', action: 'kick', reason: 'noisy' },
        { type: 'member', user: '@dave:example.org', action: 'kick', reason: null },
        { type: 'member', user: '@eve:example.org', action: 'unban', reason: null },
        { type: 'member', user: '@frank:example.org', action: 'invite', reason: null },
        { type: 'name', name: 'Parlour' },
      ]),
    )
    await user.click(within(info).getByRole('button', { name: 'Close room info' }))
    expect(screen.queryByRole('complementary', { name: 'Room info' })).toBeNull()
  })

  it('loads earlier events as their start comes into view, until it is out of view', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    for (let i = 1; i <= 2 * TIMELINE_PAGE + 10; i++) server.receive(room.id, { body: `Message ${i}` })
    await start(`/rooms/${room.id}`)
    await within(timeline()).findByText(`Message ${2 * TIMELINE_PAGE + 10}`)
    expect(within(timeline()).queryByText('Message 1')).toBeNull()
    scrolledTo(within(timeline()).getByRole('button', { name: 'Load earlier events' }).closest('li')!)
    // Page after page, while it stays in view.
    await within(timeline()).findByText('Message 1')
    expect(within(timeline()).queryByRole('button', { name: 'Load earlier events' })).toBeNull()
  })

  it('fetches events missing where a gap is, in place', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    const first = server.receive(room.id, { body: 'First' })
    const later = server.receive(room.id, { body: 'Later' })
    server.gap(room.id, first, [{ body: 'Oldest' }])
    server.gap(room.id, later, [{ body: 'Missed one' }, { body: 'Missed two' }])
    const user = await start(`/rooms/${room.id}`)
    await within(timeline()).findByText('Later')
    const gaps = within(timeline()).getAllByRole('separator', { name: 'Events missing' })
    expect(gaps).toHaveLength(2)

    // As it comes into view; or else by its button.
    scrolledTo(gaps[1])
    await within(timeline()).findByText('Missed two')
    const bodies = () =>
      within(timeline())
        .getAllByText(/^(Oldest|First|Missed one|Missed two|Later)$/)
        .map((e) => e.textContent!.trim())
    expect(bodies()).toEqual(['First', 'Missed one', 'Missed two', 'Later'])
    expect(within(timeline()).getAllByRole('separator', { name: 'Events missing' })).toHaveLength(1)

    await user.click(within(timeline()).getByRole('button', { name: 'Fetch them' }))
    await within(timeline()).findByText('Oldest')
    expect(bodies()).toEqual(['Oldest', 'First', 'Missed one', 'Missed two', 'Later'])
    expect(within(timeline()).queryByRole('separator', { name: 'Events missing' })).toBeNull()
  })

  it('sends files attached, with the message', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    const user = await start(`/rooms/${room.id}`)
    await user.upload(screen.getByLabelText('Files'), new File(['hello'], 'hello.txt', { type: 'text/plain' }))
    within(screen.getByRole('list', { name: 'Files to send' })).getByText('hello.txt')
    await user.keyboard('Here{Enter}')
    await waitFor(() =>
      expect(server.requests).toContainEqual({
        type: 'send_matrix_message',
        room: room.id,
        content: { type: 'file', blob: 'blob:hello', name: 'hello.txt', mime: 'text/plain', reply_to: null },
      }),
    )
    expect(server.requests).toContainEqual({ type: 'send_matrix_message', room: room.id, content: text('Here') })
    expect(screen.queryByRole('list', { name: 'Files to send' })).toBeNull()
  })
})

describe('switcher', () => {
  beforeEach(() => localStorage.removeItem('uplink.recent'))

  const options = () =>
    within(screen.getByRole('listbox', { name: 'Views' }))
      .getAllByRole('option')
      .map((o) => o.textContent)

  it('unfolds the nav over the view switched to, and scrolls to it', async () => {
    server.addAccount('home', ['Family', 'Family/Kids'])
    const user = await start('/mail/work/INBOX')
    const nav = within(screen.getByRole('navigation'))
    await user.click(nav.getByRole('button', { name: 'Collapse Family' }))
    await user.click(nav.getByRole('button', { name: /home@example.com/ }))
    expect(nav.queryByRole('link', { name: 'Kids' })).toBeNull()
    const scrolled = vi.spyOn(Element.prototype, 'scrollIntoView')
    await user.keyboard('{Control>}k{/Control}kids{Enter}')
    const kids = await nav.findByRole('link', { name: 'Kids' })
    expect(kids.getAttribute('aria-current')).toBe('page')
    await waitFor(() => expect(scrolled.mock.contexts).toContain(kids))
    const folded = JSON.parse(localStorage.getItem('uplink.nav.collapsed')!)
    expect(folded).not.toContain('mail:home')
    expect(folded).not.toContain('home:Family')
    scrolled.mockRestore()

    // What is in view already stays as folded as it is.
    await user.click(nav.getByRole('button', { name: 'Collapse Family' }))
    await user.keyboard('{Control>}k{/Control}home inbox{Enter}')
    await waitFor(() => expect(nav.getByRole('link', { name: /Inbox/, current: 'page' })).toBeTruthy())
    expect(nav.queryByRole('link', { name: 'Kids' })).toBeNull()
    await user.click(nav.getByRole('button', { name: 'Expand Family' }))
  })

  it('switches to the view visited before, with Ctrl+K Enter, even while typing', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    const user = await start('/mail/work/INBOX')
    await user.click(await within(screen.getByRole('navigation')).findByRole('link', { name: 'Lounge' }))
    await screen.findByRole('list', { name: 'Timeline' })
    // From the room's composer.
    await user.click(screen.getByRole('textbox', { name: /message/i }))
    await user.keyboard('{Control>}k{/Control}')
    expect(document.activeElement).toBe(screen.getByRole('textbox', { name: 'Filter views' }))
    // The most recent first, the one on show last; then the nav's order.
    expect(options()[0]).toBe('Inboxwork@example.com')
    expect(options().at(-1)).toBe(`LLounge${ME}`)
    await user.keyboard('{Enter}')
    expect(screen.queryByRole('dialog')).toBeNull()
    expect(location.pathname).toBe('/mail/work/INBOX')
    expect(localStorage.getItem('uplink.recent')).toBe(JSON.stringify(['/mail/work/INBOX', `/rooms/${room.id}`]))

    await user.keyboard('{Control>}k{/Control}')
    expect(options()[0]).toBe(`LLounge${ME}`)
    // Ctrl+K again closes it.
    await user.keyboard('{Control>}k{/Control}')
    expect(screen.queryByRole('dialog')).toBeNull()
  })

  it('moves a page at a time, keeping one option of the last page in view', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('{Control>}k{/Control}')
    // jsdom does no layout: a list showing four options.
    const list = screen.getByRole('listbox', { name: 'Views' })
    Object.defineProperty(list, 'clientHeight', { value: 100 })
    for (const option of list.children) Object.defineProperty(option, 'offsetHeight', { value: 25 })
    const highlighted = () => within(list).getAllByRole('option').findIndex((o) => o.getAttribute('aria-selected') === 'true')
    const last = within(list).getAllByRole('option').length - 1
    expect(last).toBeGreaterThan(6)
    await user.keyboard('{PageDown}')
    expect(highlighted()).toBe(3)
    await user.keyboard('{PageDown}')
    expect(highlighted()).toBe(6)
    await user.keyboard('{PageUp}')
    expect(highlighted()).toBe(3)
    await user.keyboard('{PageDown}'.repeat(last))
    expect(highlighted()).toBe(last)
    await user.keyboard('{PageUp}'.repeat(last))
    expect(highlighted()).toBe(0)
  })

  it('filters by every word typed, of the name or the account', async () => {
    server.addAccount('home', ['Projects'])
    const user = await start('/mail/work/INBOX')
    await user.keyboard('{Control>}k{/Control}')
    await user.keyboard('proj')
    expect(options()).toEqual(['Projectswork@example.com', 'Projectshome@example.com'])
    await user.keyboard(' home{ArrowDown}{ArrowUp}{Enter}')
    const nav = screen.getByRole('navigation')
    const current = nav.querySelector('[aria-current=page]')!
    expect(current.textContent).toBe('Projects')
    expect(current.closest('section')!.textContent).toContain('home@example.com')
  })
})

describe('home', () => {
  const bucketRows = (name: string) =>
    within(screen.getByRole('region', { name })).queryAllByRole('listitem').map((row) => row.querySelector('[class*=title]')!.textContent)

  it('lists the buckets, and e is done with any row, s stars it', async () => {
    server.addMatrixAccount()
    const lobby = server.addRoom('Lobby')
    server.receive(lobby.id, { body: 'Hello there' })
    server.addRoom('Quiet')
    const user = await start('/')
    expect(screen.getByRole('link', { name: /^Home/ }).getAttribute('aria-current')).toBe('page')
    await waitFor(() => expect(bucketRows('Now')).toEqual(['Lobby']))
    expect(bucketRows('Inbox')).toHaveLength(3)
    expect(within(screen.getByRole('region', { name: 'Later' })).getByText('Nothing.')).toBeTruthy()
    expect(within(screen.getByRole('navigation', { name: 'Buckets' })).getAllByRole('button').map((b) => b.textContent)).toEqual([
      'Now 1',
      'Inbox 3',
      'Later 0',
    ])

    await client.todos.create('Pay rent')
    // To-dos come first.
    await waitFor(() => expect(bucketRows('Now')).toEqual(['Pay rent', 'Lobby']))
    const focused = () => screen.getByRole('region', { name: 'Home' }).querySelector('[aria-current=true] [class*=title]')!.textContent
    expect(focused()).toBe('Pay rent')
    await user.keyboard('e')
    await waitFor(() => expect(bucketRows('Now')).toEqual(['Lobby']))
    // The room: s favourites it, e reads it.
    await user.keyboard('s')
    await waitFor(() => expect(server.rooms.find((r) => r.id === lobby.id)!.tags).toEqual(['m.favourite']))
    await user.keyboard('e')
    await waitFor(() => expect(bucketRows('Now')).toEqual([]))
    // Mail: e archives it.
    expect(focused()).toBe(bucketRows('Inbox')[0])
    await user.keyboard('e')
    await waitFor(() => expect(bucketRows('Inbox')).toHaveLength(2))
    // Opening mail opens it in a list of the bucket's.
    await user.keyboard('{Enter}')
    expect(decodeURIComponent(location.pathname)).toMatch(/^\/search\/in:inbox -\(is:unread is:room OR is:todo\)\/\d+$/)
  })

  it('shows the agenda of today and tomorrow', async () => {
    const now = new Date()
    const at = (days: number, hour: number) =>
      new Date(now.getFullYear(), now.getMonth(), now.getDate() + days, hour).toISOString()
    server.addEvent('work', 'Standup', at(1, 9), at(1, 10))
    server.addEvent('work', 'Far off', at(5, 9), at(5, 10))
    await start('/')
    const agenda = await screen.findByRole('list', { name: 'Agenda' })
    expect(agenda.textContent).toBe('Tomorrow 09:00 Standup')
  })
})

describe('search', () => {
  it('finds rooms, their messages, to-dos and events too, above the mail', async () => {
    server.addMatrixAccount()
    const lobby = server.addRoom('Lunch club')
    server.receive(lobby.id, { body: 'Lunch at noon?' })
    server.addEvent('work', 'Team lunch', '2026-11-20T12:00:00Z', '2026-11-20T13:00:00Z')
    const user = await start()
    await client.todos.create('Book lunch')
    await user.keyboard('/')
    await user.keyboard('lunch{Enter}')
    const titles = async (name: string) =>
      within(await screen.findByRole('region', { name })).getAllByRole('listitem').map((item) => item.querySelector('[class*=title]')!.textContent)
    expect(await titles('Rooms found')).toEqual(['Lunch club'])
    expect(await titles('Messages found')).toEqual(['Bob in Lunch club'])
    expect(await titles('To-dos found')).toEqual(['Book lunch'])
    expect(await titles('Events found')).toEqual(['Team lunch'])
    expect(screen.getByRole('searchbox', { name: 'Search' })).toBeTruthy()
    await user.click(within(screen.getByRole('region', { name: 'Rooms found' })).getByRole('link'))
    expect(location.pathname).toBe(`/rooms/${lobby.id}`)
  })
})

describe('to-dos', () => {
  const todoTitles = () =>
    within(screen.getByRole('list', { name: 'To-do list' }))
      .getAllByRole('listitem')
      .map((item) => item.querySelector('[class*=title]')!.textContent)
  const navCount = () => screen.getByRole('link', { name: /^To-dos/ }).textContent

  it('flags a message with Shift+t, which a to-do then follows up', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('{Shift>}t{/Shift}')
    await waitFor(() => expect(within(rows()[0]).getByTitle(/^Flagged/)).toBeTruthy())
    expect(navCount()).toBe('To-dos1')

    await user.keyboard('gk')
    expect(location.pathname).toBe('/todos')
    await waitFor(() => expect(todoTitles()).toEqual(['First']))
    // The to-do opens the message, by itself; back is back to the to-dos.
    await user.keyboard('{Enter}')
    expect(location.pathname).toMatch(/^\/message\/\d+$/)
    await screen.findByText('Body of First')
    expect(screen.getByRole('heading', { name: /First/ }).querySelector('[title^=Flagged]')).toBeTruthy()
    // Unflagging marks the to-do done.
    await user.keyboard('{Shift>}t{/Shift}')
    await waitFor(() => expect(screen.getByRole('heading', { name: /First/ }).querySelector('[title^=Flagged]')).toBeNull())
    await user.keyboard('u')
    await screen.findByRole('region', { name: 'To-dos' })
    await waitFor(() => expect(server.todos[0].done_at).not.toBeNull())
    expect(navCount()).toBe('To-dos')
  })

  it('adds with a, finishes with e, deletes with #, and renames with Enter', async () => {
    const user = await start('/todos')
    await screen.findByText('Nothing to do.')
    await user.keyboard('a')
    expect(document.activeElement).toBe(screen.getByRole('textbox', { name: 'New to-do' }))
    await user.keyboard('Call Bob{Enter}Pay rent{Enter}{Escape}')
    await waitFor(() => expect(todoTitles()).toEqual(['Call Bob', 'Pay rent']))
    expect(screen.getByRole('textbox', { name: 'New to-do' })).toHaveProperty('value', '')

    await user.keyboard('e')
    await waitFor(() => expect(todoTitles()).toEqual(['Pay rent', 'Call Bob']))
    expect(screen.getAllByRole('checkbox', { name: 'Done' }).map((c) => (c as HTMLInputElement).checked)).toEqual([
      false,
      true,
    ])

    await user.keyboard('{Enter}')
    const title = screen.getByRole('textbox', { name: 'Title' })
    expect(document.activeElement).toBe(title)
    await user.keyboard('Pay the rent{Enter}')
    await waitFor(() => expect(todoTitles()).toEqual(['Pay the rent', 'Call Bob']))

    await user.keyboard('j#')
    await waitFor(() => expect(todoTitles()).toEqual(['Pay the rent']))
  })
})

describe('history', () => {
  const items = () => within(screen.getByRole('list', { name: 'History' })).getAllByRole('listitem')

  it('lists what was done, newest first, and undoes and redoes with z', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('e')
    await waitFor(() => expect(subjects()).toEqual(['Second', 'Third']))
    await user.keyboard('gh')
    await screen.findByRole('list', { name: 'History' })
    expect(items().map((i) => i.querySelector('[class*=description]')!.textContent)).toEqual([
      server.lastDescription(),
    ])

    await user.keyboard('z')
    await waitFor(() => expect(items()).toHaveLength(2))
    expect(within(items()[0]).getByRole('button').textContent).toBe('Redo')
    expect(within(items()[1]).queryByRole('button')).toBeNull()
    expect(items()[1].textContent).toContain('undone')
    await user.keyboard('gi')
    await waitFor(() => expect(subjects()).toEqual(['First', 'Second', 'Third']))
  })
})

describe('matrix security', () => {
  beforeEach(() => server.addMatrixAccount())

  it('shows the device and its trust, and recovers with the recovery key', async () => {
    const user = await start('/matrix/mx/security')
    const page = await screen.findByRole('region', { name: 'This device' })
    await within(page).findByText(/Not verified/)
    const devices = screen.getByRole('region', { name: 'Devices' })
    within(devices).getByText('Element Android')
    within(within(devices).getByText('uplink (mx)').closest('tr')!).getByText('Not verified')

    const recover = screen.getByRole('region', { name: 'Recover' })
    await user.type(within(recover).getByLabelText('Recovery key or passphrase'), 'wrong')
    await user.click(within(recover).getByRole('button', { name: 'Recover' }))
    await screen.findByRole('alert')
    expect(screen.getByRole('alert').textContent).toContain('wrong key')

    await user.clear(within(recover).getByLabelText('Recovery key or passphrase'))
    await user.type(within(recover).getByLabelText('Recovery key or passphrase'), RECOVERY_KEY)
    await user.click(within(recover).getByRole('button', { name: 'Recover' }))
    await within(page).findByText(/Verified by the account/)
    expect(screen.queryByRole('region', { name: 'Recover' })).toBeNull()
    within(page).getByText('In use')
  })

  it('imports and exports room keys', async () => {
    const user = await start('/matrix/mx/security')
    const importing = await screen.findByRole('form', { name: 'Import room keys' })
    await user.upload(within(importing).getByLabelText('Exported keys'), new File(['keys'], 'element-keys.txt'))
    await user.type(within(importing).getByLabelText('Import passphrase'), 'export passphrase')
    await user.click(within(importing).getByRole('button', { name: 'Import' }))
    await within(importing).findByText('Imported 3 new keys of 5.')

    const exporting = screen.getByRole('form', { name: 'Export room keys' })
    const button = within(exporting).getByRole('button', { name: 'Export' })
    await user.type(within(exporting).getByLabelText('Export passphrase'), 'secret')
    expect(button).toHaveProperty('disabled', true)
    await user.type(within(exporting).getByLabelText('Export passphrase again'), 'secret')
    const clicked = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {})
    await user.click(button)
    await waitFor(() => expect(clicked).toHaveBeenCalled())
    expect((clicked.mock.contexts[0] as HTMLAnchorElement).download).toBe('element-keys-mx.txt')
  })

  it('verifies with another device by comparing emoji', async () => {
    const user = await start('/matrix/mx/security')
    const page = await screen.findByRole('region', { name: 'This device' })
    await user.click(within(page).getByRole('button', { name: 'Verify with another device' }))
    const verification = await screen.findByRole('region', { name: 'Verification' })
    within(verification).getByText(/Waiting for another device of the account to accept/)
    expect(within(page).getByRole('button', { name: 'Verify with another device' })).toHaveProperty('disabled', true)

    const emoji = [
      { symbol: '🐶', description: 'Dog' },
      { symbol: '🔑', description: 'Key' },
    ]
    server.verificationChanged('mx', 'flow1', {
      other_device: 'PHONE',
      phase: { type: 'compare', emoji, decimals: [1234, 5678, 9012] },
    })
    const shown = await within(verification).findByRole('list', { name: 'Emoji to compare' })
    expect(within(shown).getAllByRole('listitem').map((li) => li.textContent)).toEqual(['🐶Dog', '🔑Key'])
    within(verification).getByText(/Check that Element Android shows these/)
    within(verification).getByText('Or these numbers: 1234 5678 9012')
    await user.click(within(verification).getByRole('button', { name: 'They match' }))
    await within(verification).findByText('Waiting for Element Android to confirm…')
    expect(server.requests).toContainEqual({ type: 'answer_verification', account: 'mx', flow_id: 'flow1', answer: 'confirm' })

    const security = server.security.get('mx')!
    server.securityChanged('mx', {
      ...security,
      verified: true,
      verifications: security.verifications.map((v) => ({ ...v, phase: { type: 'done' } })),
    })
    await within(verification).findByText('Verified with Element Android.')
    within(page).getByText(/Verified by the account/)
  })

  it('answers another device asking to verify, which the nav counts', async () => {
    const user = await start('/matrix/mx/security')
    await screen.findByText('Element Android')
    server.verificationAsked('mx', 'theirs', 'PHONE')
    const verification = await screen.findByRole('region', { name: 'Verification' })
    within(verification).getByText('Element Android asks to verify this device.')
    const securityLink = screen.getByText('Security').closest('a')!
    await waitFor(() => expect(securityLink.textContent).toBe('Security1'))

    await user.click(within(verification).getByRole('button', { name: 'Accept' }))
    await within(verification).findByText(/^Starting to compare with Element Android…/)
    await waitFor(() => expect(securityLink.textContent).toBe('Security'))
    await user.click(within(verification).getByRole('button', { name: 'Cancel' }))
    await within(verification).findByText('Cancelled: The user cancelled the verification.')
  })

  it('loads again when the client says it changed', async () => {
    await start('/matrix/mx/security')
    await screen.findByText('Element Android')
    const security = server.security.get('mx')!
    server.securityChanged('mx', {
      ...security,
      devices: [...security.devices, { ...security.devices[1]!, device_id: 'LAPTOP', name: 'Element Desktop' }],
    })
    await screen.findByText('Element Desktop')
  })
})
