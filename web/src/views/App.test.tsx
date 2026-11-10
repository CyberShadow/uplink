import { signal } from '@preact/signals'
import { fireEvent, render, screen, waitFor, within } from '@testing-library/preact'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it } from 'vitest'
import type { Entry } from '../generated/protocol'
import { Client } from '../stores/client'
import { FakeServer } from '../test/server'
import { App } from './App'

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

async function start(path = '/') {
  history.replaceState(null, '', path)
  render(<App client={client} status={signal('open')} />)
  client.reset()
  if (path.startsWith('/rooms/')) await screen.findByRole('list', { name: 'Timeline' })
  else if (path === '/todos') await screen.findByRole('region', { name: 'To-dos' })
  else if (path === '/history') await screen.findByRole('list', { name: 'History' })
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
  it('opens the first Inbox, with labels in the nav', async () => {
    await start()
    expect(location.pathname).toBe('/mail/work/INBOX')
    expect(subjects()).toEqual(['First', 'Second', 'Third'])
    const nav = screen.getByRole('navigation')
    expect(within(nav).getByRole('link', { name: /Inbox/ }).getAttribute('aria-current')).toBe('page')
    expect(within(nav).getByRole('link', { name: 'Projects' })).toBeTruthy()
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
    const second = client.mail.list.value!.entries.find((e) => e.subject === 'Second')!
    expect(server.labelsOf(second.id)).not.toContain(server.labelId('work', 'UNREAD'))
    // Reading quietly: no notice.
    expect(screen.queryByRole('status')).toBeNull()

    await user.keyboard('j')
    expect(await screen.findByText('Body of Third')).toBeTruthy()
    await user.keyboard('u')
    await screen.findByRole('list', { name: 'Messages' })
    expect(focusedSubject()).toBe('Third')
  })

  it('shows an HTML body in a sandbox, remote images only when asked', async () => {
    const first = server.deliver('work', 'Newsletter')
    server.html.set(first.message, '<p>Hello <img src="https://t.example/pixel.gif"></p>')
    const user = await start('/mail/work/INBOX')
    await user.keyboard('{Enter}')
    const frame = (await screen.findByTitle('Message body')) as HTMLIFrameElement
    expect(frame.getAttribute('sandbox')).not.toContain('allow-scripts')
    expect(frame.srcdoc).toContain('<p>Hello <img src="https://t.example/pixel.gif"></p>')
    expect(frame.srcdoc).toContain("img-src data:;")
    await user.click(screen.getByRole('button', { name: 'Show them' }))
    expect(frame.srcdoc).toContain('img-src data: https: http:')
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
    const first = client.mail.list.value!.entries[0]
    expect(server.labelsOf(first.id)).toContain(server.labelId('work', 'Travel'))
    expect(within(rows()[0]).getByText('Travel')).toBeTruthy()
  })

  it('moves to a label with v, out of the label on show', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('v')
    const dialog = screen.getByRole('dialog', { name: 'Move to' })
    expect(within(dialog).getAllByRole('option').map((o) => o.textContent)).toEqual(['Projects', 'Travel'])
    const first = client.mail.list.value!.entries[0]
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
    const first = client.mail.list.value!.entries[0]
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
    const selected = () => [...client.mail.selected.value].map((id) => client.mail.list.value!.entries.find((e) => e.id === id)!.subject)
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
    expect(server.labelsOf(client.mail.list.value!.entries[0].id)).toContain(server.labelId('work', 'INBOX'))
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
    expect(server.labelsOf(client.mail.list.value!.entries[1].id)).toContain(server.labelId('work', 'STARRED'))
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

  it('jumps to labels with g', async () => {
    server.deliver('work', 'Outgoing', ['SENT'])
    const user = await start('/mail/work/INBOX')
    await user.keyboard('gt')
    expect(location.pathname).toBe('/mail/work/SENT')
    await screen.findByText('Outgoing')
  })

  it('takes g s as a sequence, without starring the message as s alone would', async () => {
    const user = await start('/mail/work/INBOX')
    const first = client.mail.list.value!.entries[0]
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
      'Inbox/Projects',
      'Red',
    ])
    expect(within(screen.getByRole('navigation')).getByRole('link', { name: 'Gmail drafts' })).toBeTruthy()
  })

  it('opens every account’s Inbox as one, saying whose each message is', async () => {
    const user = await start('/')
    expect(location.pathname).toBe('/inbox')
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
    expect(draft.in_reply_to).toBe(client.mail.list.value!.entries[0].message)
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

describe('matrix', () => {
  it('lists rooms in the nav, and reading one marks it read', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Lounge')
    server.receive(room.id, { body: 'Hello there' })
    const user = await start()
    const nav = screen.getByRole('navigation')
    const link = await within(nav).findByRole('link', { name: /^Lounge/ })
    expect(link.textContent).toBe('Lounge1')
    await user.click(link)
    expect(location.pathname).toBe(`/rooms/${room.id}`)
    expect(await screen.findByText('Hello there')).toBeTruthy()
    await waitFor(() => expect(link.textContent).toBe('Lounge'))
    expect(link.getAttribute('aria-current')).toBe('page')
    // Reading quietly: no notice.
    expect(screen.queryByRole('status')).toBeNull()
  })

  it('joins an invitation, and leaves the room, with undo in between', async () => {
    server.addMatrixAccount()
    const room = server.addRoom('Party', false, 'invited')
    const user = await start(`/rooms/${room.id}`)
    const nav = screen.getByRole('navigation')
    expect(within(nav).getByRole('link', { name: /^Party/ }).textContent).toBe('Party (invited)')
    expect(screen.queryByRole('textbox', { name: /message/i })).toBeNull()

    await user.click(screen.getByRole('button', { name: 'Join' }))
    await screen.findByText('Joining…')
    // Changed one's mind before it took effect.
    await user.click(within(screen.getByRole('status')).getByRole('button', { name: 'Undo' }))
    await screen.findByRole('button', { name: 'Join' })

    await user.click(screen.getByRole('button', { name: 'Join' }))
    server.settleMembership(room.id)
    await screen.findByRole('button', { name: 'Leave' })
    expect(within(nav).getByRole('link', { name: /^Party/ }).textContent).toBe('Party')

    await user.click(screen.getByRole('button', { name: 'Leave' }))
    await screen.findByText('Leaving…')
    server.settleMembership(room.id)
    await screen.findByText('No such room.')
    expect(within(nav).queryByRole('link', { name: /^Party/ })).toBeNull()
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
    expect(server.requests).toContainEqual({ type: 'send_matrix_message', room: room.id, body: 'First line\nsecond line' })

    server.echo(server.matrixOutbox[0].id)
    await waitFor(() => expect(screen.queryByText('Waiting to send')).toBeNull())
    expect(screen.getByText('First line second line')).toBeTruthy()
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
