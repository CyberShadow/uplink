import { signal } from '@preact/signals'
import { render, screen, waitFor, within } from '@testing-library/preact'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it } from 'vitest'
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
  await screen.findByRole('list', { name: 'Messages' })
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

  it('shows the shortcuts with ?', async () => {
    const user = await start('/mail/work/INBOX')
    await user.keyboard('?')
    const help = screen.getByRole('dialog', { name: 'Keyboard shortcuts' })
    expect(within(help).getByText('Archive')).toBeTruthy()
    await user.keyboard('{Escape}')
    expect(screen.queryByRole('dialog')).toBeNull()
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
})
