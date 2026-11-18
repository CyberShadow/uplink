import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { Client } from '../stores/client'
import { FakeServer } from '../test/server'
import { EMPTY } from './reply'
import type { ComposeSession } from '../stores/drafts'
import { SAVE_DELAY_MS } from './session'

let server: FakeServer
let client: Client
/** The draft last opened. */
let draft: string

beforeEach(async () => {
  vi.useFakeTimers()
  server = new FakeServer()
  server.addAccount('work')
  client = new Client(server)
  server.attach(client)
  client.reset()
  await vi.waitFor(() => expect(client.drafts.drafts.value).not.toBeNull())
})

afterEach(() => {
  vi.useRealTimers()
})

async function open(body = ''): Promise<ComposeSession> {
  draft = (await client.drafts.create('work', { ...EMPTY, body }, null))!
  await vi.waitFor(() => expect(client.drafts.drafts.value?.has(draft)).toBe(true))
  return client.drafts.session(draft)
}

const serverBody = () => server.drafts.get(draft)!.content.body
const edits = () => server.requests.filter((r) => r.type === 'edit_draft')

describe('ComposeSession', () => {
  it('saves typing once it pauses, as a splice', async () => {
    const session = await open('Hello')
    session.set('body', 'Hello w')
    session.set('body', 'Hello world')
    expect(session.state.value).toBe('unsaved')
    await vi.advanceTimersByTimeAsync(SAVE_DELAY_MS)
    expect(edits()).toEqual([
      { type: 'edit_draft', draft, revision: 0, field: 'body', splice: { offset: 5, delete: '', insert: ' world' } },
    ])
    expect(serverBody()).toBe('Hello world')
    expect(session.state.value).toBe('saved')
  })

  it('shows edits from elsewhere', async () => {
    const session = await open('Hello')
    const rebased = vi.fn()
    session.onRebase = rebased
    server.editElsewhere(draft, 'body', { offset: 0, delete: '', insert: 'Oh, ' })
    expect(session.content.value.body).toBe('Oh, Hello')
    expect(rebased).toHaveBeenCalledWith('body', 'Hello', 'Oh, Hello')
    expect(session.state.value).toBe('saved')
  })

  it('rebases unsaved typing onto edits from elsewhere', async () => {
    const session = await open('Hello')
    session.set('body', 'Hello world')
    server.editElsewhere(draft, 'body', { offset: 0, delete: '', insert: 'Oh, ' })
    expect(session.content.value.body).toBe('Oh, Hello world')
    await vi.advanceTimersByTimeAsync(SAVE_DELAY_MS)
    expect(serverBody()).toBe('Oh, Hello world')
    expect(session.state.value).toBe('saved')
  })

  it('resends an edit made stale on its way', async () => {
    const session = await open('Hello')
    session.set('subject', 'Hi')
    server.meanwhile = () =>
      server.editElsewhere(draft, 'body', { offset: 5, delete: '', insert: '!' })
    await vi.advanceTimersByTimeAsync(SAVE_DELAY_MS)
    await vi.waitFor(() => expect(session.state.value).toBe('saved'))
    expect(edits().map((e) => e.type === 'edit_draft' && e.revision)).toEqual([0, 1])
    expect(server.drafts.get(draft)!.content).toMatchObject({ subject: 'Hi', body: 'Hello!' })
    expect(session.content.value).toMatchObject({ subject: 'Hi', body: 'Hello!' })
  })

  it('keeps typing that happens while an edit is in flight', async () => {
    const session = await open('')
    session.set('body', 'a')
    server.meanwhile = () => session.set('body', 'ab')
    await vi.advanceTimersByTimeAsync(SAVE_DELAY_MS)
    await vi.waitFor(() => expect(serverBody()).toBe('ab'))
    expect(session.state.value).toBe('saved')
  })

  it('flushes before sending', async () => {
    const session = await open('')
    session.set('to', 'bob@example.org')
    await session.flush()
    expect(server.drafts.get(draft)!.content.to).toBe('bob@example.org')
    await client.drafts.send(draft)
    await vi.waitFor(() => expect(session.state.value).toBe('gone'))
    await vi.waitFor(() => expect(client.drafts.outbox.value).toHaveLength(1))
  })
})
