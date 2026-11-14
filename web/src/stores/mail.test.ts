import { beforeEach, describe, expect, it, vi } from 'vitest'
import { FakeServer } from '../test/server'
import { Client } from './client'
import type { MailStore } from './mail'
import { PAGE, conversationKey, threadOf } from './mail'

let server: FakeServer
let client: Client
let mail: MailStore

beforeEach(() => {
  server = new FakeServer()
  server.addAccount('work', ['Projects'])
  client = new Client(server)
  mail = client.mail
  server.attach(client)
})

const subjects = () => mail.list.value?.conversations.map((c) => c.latest.subject)
const focused = () => mail.list.value?.conversations.find((c) => conversationKey(c) === mail.focus.value)?.latest.subject

async function showInbox() {
  mail.reset()
  mail.show({ kind: 'label', account: 'work', label: 'INBOX' })
  await vi.waitFor(() => expect(mail.list.value?.loaded).toBe(true))
}

describe('MailStore', () => {
  it('lists a label newest first, focusing the first entry', async () => {
    server.deliver('work', 'Older')
    server.deliver('work', 'Newer')
    server.deliver('work', 'Sent one', ['SENT'])
    await showInbox()
    expect(subjects()).toEqual(['Newer', 'Older'])
    expect(focused()).toBe('Newer')
  })

  it('archives, moving focus on, and undoes the archive', async () => {
    server.deliver('work', 'Third')
    const second = server.deliver('work', 'Second')
    server.deliver('work', 'First')
    await showInbox()
    mail.moveFocus(1)
    expect(focused()).toBe('Second')

    await mail.archive(mail.targets.value.map(threadOf))
    expect(server.labelsOf(second.id)).not.toContain(server.labelId('work', 'INBOX'))
    expect(subjects()).toEqual(['First', 'Third'])
    expect(focused()).toBe('Third')
    const notice = client.ops.notice.value
    expect(notice).toMatchObject({ kind: 'done', description: 'Changed 1 messages' })

    await client.ops.undo(notice!.kind === 'done' ? notice!.id : -1)
    await vi.waitFor(() => expect(subjects()).toEqual(['First', 'Second', 'Third']))
    expect(client.ops.notice.value).toMatchObject({ kind: 'done', description: 'Undid it' })
  })

  it('reports a failed op', async () => {
    server.deliver('work', 'Only')
    await showInbox()
    await client.ops.undo(99)
    expect(client.ops.notice.value).toEqual({ kind: 'failed', message: 'not undoable' })
  })

  it('follows label changes observed on the provider', async () => {
    const lunch = server.deliver('work', 'Lunch')
    await showInbox()
    mail.show({ kind: 'label', account: 'work', label: 'Label_1' })
    await vi.waitFor(() => expect(mail.list.value?.loaded).toBe(true))
    expect(subjects()).toEqual([])

    server.relabel(lunch.id, server.labelId('work', 'Projects'), true)
    await vi.waitFor(() => expect(subjects()).toEqual(['Lunch']))
    server.relabel(lunch.id, server.labelId('work', 'UNREAD'), false)
    await vi.waitFor(() =>
      expect(mail.list.value!.conversations[0].latest.labels).not.toContain(server.labelId('work', 'UNREAD')),
    )
    await vi.waitFor(() =>
      expect(mail.labels.value.get(server.labelId('work', 'INBOX'))?.unread).toBe(0),
    )
  })

  it('reloads for new mail, keeping focus where it was', async () => {
    server.deliver('work', 'Second')
    server.deliver('work', 'First')
    await showInbox()
    mail.moveFocus(1)

    const added = server.deliver('work', 'New')
    server.observe('work', [{ type: 'entries_added', entries: [added.id] }])
    await vi.waitFor(() => expect(subjects()).toEqual(['New', 'First', 'Second']))
    expect(focused()).toBe('Second')
  })

  it('drops removed entries', async () => {
    const gone = server.deliver('work', 'Gone')
    server.deliver('work', 'Kept')
    await showInbox()
    server.remove(gone)
    await vi.waitFor(() => expect(subjects()).toEqual(['Kept']))
  })

  it('loads further pages', async () => {
    for (let i = 0; i < PAGE + 5; i++) server.deliver('work', `Message ${i}`)
    await showInbox()
    expect(mail.list.value!.conversations).toHaveLength(PAGE)
    mail.loadMore()
    await vi.waitFor(() => expect(mail.list.value!.conversations).toHaveLength(PAGE + 5))
    expect(mail.list.value!.more).toBe(false)
  })

  it('acts on the conversations of several accounts as one op', async () => {
    server.addAccount('home')
    const work = server.deliver('work', 'Report at work')
    const home = server.deliver('home', 'Report at home')
    mail.reset()
    mail.show({ kind: 'search', query: 'report' })
    await vi.waitFor(() => expect(subjects()).toHaveLength(2))
    mail.selectAll()

    await mail.archive(mail.targets.value.map(threadOf))
    const ops = server.requests.filter((r) => r.type === 'act_on_conversations')
    expect(ops).toEqual([
      {
        type: 'act_on_conversations',
        conversations: { type: 'listed', conversations: [[home.id], [work.id]] },
        action: { type: 'archive' },
        shown: false,
      },
    ])
    expect(server.labelsOf(work.id)).not.toContain(server.labelId('work', 'INBOX'))
    expect(server.labelsOf(home.id)).not.toContain(server.labelId('home', 'INBOX'))
    // A search lists entries whatever their labels.
    expect(subjects()).toHaveLength(2)
  })

  it('subscribes to lists as queries of mail', async () => {
    server.addAccount('a "b"')
    mail.reset()
    const queries = () => server.requests.flatMap((r) => (r.type === 'subscribe' ? [r.query] : []))
    for (const view of [
      { kind: 'label', account: 'work', label: 'Label_1' },
      { kind: 'all', account: 'a "b"' },
      { kind: 'inbox' },
    ] as const) {
      mail.show(view)
      await vi.waitFor(() => expect(mail.list.value?.loaded).toBe(true))
    }
    expect(queries()).toEqual(['is:mail account:"work" label:"Projects"', 'is:mail account:"a \\"b\\""', 'is:mail in:inbox'])
    // Each, left, is unsubscribed from.
    expect(server.requests.filter((r) => r.type === 'unsubscribe')).toHaveLength(2)
  })

  it('follows a search as labels change, in the scope the server lists', async () => {
    const old = server.deliver('work', 'Old report', ['TRASH'])
    const kept = server.deliver('work', 'Kept report', ['INBOX'])
    mail.reset()
    mail.show({ kind: 'search', query: 'report in:trash' })
    await vi.waitFor(() => expect(subjects()).toEqual(['Old report']))
    expect(mail.list.value!.scope).toEqual({ type: 'within', labels: [server.labelId('work', 'TRASH')] })

    // Out of Trash, out of the list; what joins it, the server says.
    server.relabel(old.id, server.labelId('work', 'TRASH'), false)
    await vi.waitFor(() => expect(subjects()).toEqual([]))
    server.relabel(kept.id, server.labelId('work', 'TRASH'), true)
    await vi.waitFor(() => expect(subjects()).toEqual(['Kept report']))
  })

  it('keeps its place deep in a list, as the open conversation leaves it and mail comes', async () => {
    const count = 6 * PAGE + 50
    for (let i = 0; i < count; i++) server.deliver('work', `Starred ${i}`, ['INBOX', 'STARRED'])
    mail.reset()
    mail.show({ kind: 'search', query: 'label:starred' })
    await vi.waitFor(() => expect(subjects()).toHaveLength(PAGE))
    // To the end, past what a page may be.
    while (mail.list.value!.more) {
      const loaded = subjects()!.length
      mail.loadMore()
      await vi.waitFor(() => expect(subjects()!.length).toBeGreaterThan(loaded))
    }
    expect(subjects()).toHaveLength(count)

    const deep = mail.list.value!.conversations[600]
    mail.focusOn(deep)
    mail.showConversation(deep.latest.id)
    await mail.setStar([threadOf(deep)], false)
    expect(server.labelsOf(deep.latest.id)).not.toContain(server.labelId('work', 'STARRED'))
    const newest = server.deliver('work', 'Newest', ['INBOX', 'STARRED'])
    server.observe('work', [{ type: 'entries_added', entries: [newest.id] }])
    await vi.waitFor(() => expect(subjects()![0]).toBe('Newest'))
    // Open, it stays where it was; nothing else goes.
    expect(subjects()).toHaveLength(count + 1)
    expect(focused()).toBe('Starred 49')

    // Closed, it leaves, and the focus is on the next.
    mail.showConversation(null)
    expect(subjects()).toHaveLength(count)
    expect(subjects()).not.toContain('Starred 49')
    expect(focused()).toBe('Starred 48')
  })

  it('keeps the open conversation in a search it leaves, until it closes', async () => {
    const read = server.deliver('work', 'Read on open')
    server.deliver('work', 'Still unread')
    mail.reset()
    mail.show({ kind: 'search', query: 'label:unread' })
    await vi.waitFor(() => expect(subjects()).toEqual(['Still unread', 'Read on open']))
    mail.showConversation(read.id)
    const opened = mail.list.value!.conversations.find((c) => c.latest.id === read.id)!
    mail.markRead([threadOf(opened)], true)
    // Reloaded, the server lists it no more; open, it stays where it was.
    server.deliver('work', 'Newest')
    await vi.waitFor(() => expect(subjects()).toEqual(['Newest', 'Still unread', 'Read on open']))
    mail.showConversation(null)
    await vi.waitFor(() => expect(subjects()).toEqual(['Newest', 'Still unread']))
  })
})
