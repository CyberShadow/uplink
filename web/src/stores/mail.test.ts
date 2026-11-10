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
    expect(mail.list.value!.conversations[0].latest.labels).not.toContain(server.labelId('work', 'UNREAD'))
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
    server.observe('work', [{ type: 'entries_removed', entries: [gone.id] }])
    expect(subjects()).toEqual(['Kept'])
  })

  it('loads further pages', async () => {
    for (let i = 0; i < PAGE + 5; i++) server.deliver('work', `Message ${i}`)
    await showInbox()
    expect(mail.list.value!.conversations).toHaveLength(PAGE)
    mail.loadMore()
    await vi.waitFor(() => expect(mail.list.value!.conversations).toHaveLength(PAGE + 5))
    expect(mail.list.value!.next).toBeNull()
  })

  it('applies one op per account', async () => {
    server.addAccount('home')
    server.deliver('work', 'Report at work')
    server.deliver('home', 'Report at home')
    mail.reset()
    mail.show({ kind: 'search', query: 'report' })
    await vi.waitFor(() => expect(subjects()).toHaveLength(2))
    mail.selectAll()

    await mail.archive(mail.targets.value.map(threadOf))
    const ops = server.requests.filter((r) => r.type === 'modify_labels')
    expect(ops).toEqual([
      expect.objectContaining({ remove: [server.labelId('home', 'INBOX')] }),
      expect.objectContaining({ remove: [server.labelId('work', 'INBOX')] }),
    ])
    // A search lists entries whatever their labels.
    expect(subjects()).toHaveLength(2)
  })
})
